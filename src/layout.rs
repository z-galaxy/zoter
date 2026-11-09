//! How a hand-over is laid out in its memfd: [`Layout`] writes it, and [`read`] reads it.
//!
//! The integers of the memfd are little-endian, and its offsets are from its start, so that it can
//! be anywhere in memory, at any address. Its header is:
//!
//! ```text
//!      0  8 bytes  `MAGIC`
//!      8  2 bytes  the major version of the format, `MAJOR`
//!     10  2 bytes  how long the header is, a multiple of 8: a later minor version of the format
//!                  adds fields at its end, which an earlier one skips
//!     12  4 bytes  the features of the format that the memfd needs, as bits, which a version of
//!                  zoter that doesn't know one of refuses: none yet (`KNOWN_FEATURES`)
//!     16  8 bytes  how long the memfd is
//!     24  8 bytes  how many items the table has
//!     32  8 bytes  how many file descriptors go with the memfd: those of the state, then those
//!                  of the items, in the order of the table
//!     40  8 bytes  the offset of the record of the state
//!     48  8 bytes  the length of the record of the state
//!     56  8 bytes  how many file descriptors the state has, from the first on
//! ```
//!
//! The table comes next. It has 40 bytes for each item, in 8 bytes each: its id, the offset and
//! the length of its record, and the index of its first file descriptor and how many it has. Then
//! come the record of the state and those of the items, each at an offset that is a multiple of 8,
//! with zeros up to the next one, and up to the end of the memfd.

use std::{
    io::{self, ErrorKind},
    ops::Range,
};

/// Where the parts of a hand-over go in its memfd.
#[derive(Debug)]
pub(crate) struct Layout {
    /// Where the record of the state goes.
    state: Range<usize>,
    /// Where the record of each item goes, in the order of the items.
    items: Vec<Range<usize>>,
    /// How long the memfd is.
    len: usize,
}

impl Layout {
    /// The layout of a hand-over with a record of `state_len` bytes for its state, and a record of
    /// each of `item_lens` for each of its items. It fails with `ErrorKind::InvalidInput` for more
    /// bytes than a memfd can be long.
    pub(crate) fn new<I>(state_len: usize, item_lens: I) -> io::Result<Self>
    where
        I: ExactSizeIterator<Item = usize>,
    {
        let table_len = item_lens
            .len()
            .checked_mul(ENTRY_LEN)
            .ok_or_else(too_long)?;
        let mut end = HEADER_LEN.checked_add(table_len).ok_or_else(too_long)?;
        let mut place = |len: usize| -> io::Result<Range<usize>> {
            let start = end;
            end = start
                .checked_add(len)
                .and_then(|end| end.checked_next_multiple_of(8))
                .ok_or_else(too_long)?;

            Ok(start..start + len)
        };
        let state = place(state_len)?;
        let items = item_lens.map(&mut place).collect::<io::Result<_>>()?;
        // A memfd is no longer than an `i64` can count, nor a slice longer than an `isize` can.
        if isize::try_from(end).is_err() {
            return Err(too_long());
        }

        Ok(Self {
            state,
            items,
            len: end,
        })
    }

    /// How long the memfd is.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Where the record of the state goes.
    pub(crate) fn state(&self) -> Range<usize> {
        self.state.clone()
    }

    /// Where the record of the item at `index` goes.
    pub(crate) fn item(&self, index: usize) -> Range<usize> {
        self.items[index].clone()
    }

    /// Write the header and the table into `memfd`, which is [`len`](Self::len) bytes long, and
    /// zeroed but for the records: `state_fds` is how many file descriptors the state has, and
    /// `entries` are the items, where their records are as this layout has them, which may leave
    /// some out, as those whose records failed to be saved.
    pub(crate) fn write(&self, memfd: &mut [u8], state_fds: usize, entries: &[Entry]) {
        let fds = state_fds as u64 + entries.iter().map(Entry::fd_count).sum::<u64>();
        let mut header = Writer(&mut memfd[..HEADER_LEN]);
        header.put(MAGIC);
        header.put(&MAJOR.to_le_bytes());
        header.put(&(HEADER_LEN as u16).to_le_bytes());
        header.put(&0u32.to_le_bytes());
        for value in [
            self.len as u64,
            entries.len() as u64,
            fds,
            self.state.start as u64,
            self.state.len() as u64,
            state_fds as u64,
        ] {
            header.put(&value.to_le_bytes());
        }

        let table_len = entries.len() * ENTRY_LEN;
        let mut table = Writer(&mut memfd[HEADER_LEN..HEADER_LEN + table_len]);
        for entry in entries {
            for value in [
                entry.id,
                entry.record.start as u64,
                entry.record.len() as u64,
                entry.fds.start,
                entry.fd_count(),
            ] {
                table.put(&value.to_le_bytes());
            }
        }
    }
}

/// An item of a hand-over, as the table of its memfd has it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The id of the item.
    pub(crate) id: u64,
    /// Where its record is in the memfd.
    pub(crate) record: Range<usize>,
    /// Where its file descriptors are among those that go with the memfd.
    pub(crate) fds: Range<u64>,
}

impl Entry {
    /// How many file descriptors the item has.
    fn fd_count(&self) -> u64 {
        self.fds.end - self.fds.start
    }
}

/// What a memfd holds of a hand-over, as [`read`] reads it.
#[derive(Debug)]
pub(crate) struct Contents {
    /// Where the record of the state is in the memfd.
    pub(crate) state: Range<usize>,
    /// How many file descriptors the state has, from the first on.
    pub(crate) state_fds: u64,
    /// The items.
    pub(crate) entries: Vec<Entry>,
}

/// What `memfd`, all of the bytes of the memfd of a hand-over, holds.
///
/// It fails with `ErrorKind::Unsupported` for a memfd of a later major version of the format, or
/// that needs a feature of it that this version of zoter doesn't know, and with
/// `ErrorKind::InvalidData` for anything else that is no hand-over that [`Layout`] laid out.
pub(crate) fn read(memfd: &[u8]) -> io::Result<Contents> {
    if memfd.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(malformed());
    }
    match u16::from_le_bytes(field(memfd, 8)?) {
        MAJOR => (),
        major if major > MAJOR => {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!("the hand-over is of version {major} of its format, later than {MAJOR}"),
            ));
        }
        _ => return Err(malformed()),
    }
    let header_len = usize::from(u16::from_le_bytes(field(memfd, 10)?));
    if header_len % 8 != 0 || header_len < HEADER_LEN {
        return Err(malformed());
    }
    if u32::from_le_bytes(field(memfd, 12)?) & !KNOWN_FEATURES != 0 {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "the hand-over needs a feature of its format that this version of zoter doesn't know",
        ));
    }
    if number(memfd, 16)? != memfd.len() as u64 {
        return Err(malformed());
    }
    let count = usize::try_from(number(memfd, 24)?).map_err(|_| malformed())?;
    let fds = number(memfd, 32)?;
    let state = record(memfd, number(memfd, 40)?, number(memfd, 48)?)?;
    let state_fds = number(memfd, 56)?;
    if state_fds > fds {
        return Err(malformed());
    }

    let table_len = count.checked_mul(ENTRY_LEN).ok_or_else(malformed)?;
    let table = header_len
        .checked_add(table_len)
        .and_then(|end| memfd.get(header_len..end))
        .ok_or_else(malformed)?;
    let entries = table
        .as_chunks::<ENTRY_LEN>()
        .0
        .iter()
        .map(|entry| -> io::Result<Entry> {
            let first = number(entry, 24)?;
            let end = first
                .checked_add(number(entry, 32)?)
                .ok_or_else(malformed)?;
            if end > fds {
                return Err(malformed());
            }

            Ok(Entry {
                id: number(entry, 0)?,
                record: record(memfd, number(entry, 8)?, number(entry, 16)?)?,
                fds: first..end,
            })
        })
        .collect::<io::Result<_>>()?;

    Ok(Contents {
        state,
        state_fds,
        entries,
    })
}

/// Memory that a header or a table is copied into, from its start on.
struct Writer<'d>(&'d mut [u8]);

impl Writer<'_> {
    /// Copy `bytes` into the memory, after what was copied into it before.
    fn put(&mut self, bytes: &[u8]) {
        let (head, rest) = std::mem::take(&mut self.0).split_at_mut(bytes.len());
        head.copy_from_slice(bytes);
        self.0 = rest;
    }
}

/// The `N` bytes of `bytes` at `at`.
fn field<const N: usize>(bytes: &[u8], at: usize) -> io::Result<[u8; N]> {
    let field = at
        .checked_add(N)
        .and_then(|end| bytes.get(at..end))
        .ok_or_else(malformed)?;

    Ok(field
        .try_into()
        .expect("the bytes of a field are as many as it has"))
}

/// The number of 8 bytes of `bytes` at `at`.
fn number(bytes: &[u8], at: usize) -> io::Result<u64> {
    field(bytes, at).map(u64::from_le_bytes)
}

/// Where the record at `offset`, of `len` bytes, is in `memfd`, if it is all there.
fn record(memfd: &[u8], offset: u64, len: u64) -> io::Result<Range<usize>> {
    let start = usize::try_from(offset).map_err(|_| malformed())?;
    let end = usize::try_from(len)
        .ok()
        .and_then(|len| start.checked_add(len))
        .ok_or_else(malformed)?;
    if end > memfd.len() {
        return Err(malformed());
    }

    Ok(start..end)
}

/// The error of a memfd that holds no hand-over that [`Layout`] laid out.
fn malformed() -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        "the memfd of the hand-over isn't one that zoter laid out",
    )
}

/// The error of a hand-over too long for a memfd.
fn too_long() -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        "the hand-over is longer than a memfd can be",
    )
}

/// The bytes that the memfd of a hand-over starts with.
const MAGIC: &[u8; 8] = b"zoter\0\0\0";
/// The major version of the format, which a version of zoter refuses a memfd of a later one of, as
/// it can't read it.
const MAJOR: u16 = 1;
/// The features of the format that this version of zoter knows, as the bits of those that a memfd
/// needs: none yet.
const KNOWN_FEATURES: u32 = 0;
/// How long the header is.
const HEADER_LEN: usize = 64;
/// How long the entry of an item is in the table.
const ENTRY_LEN: usize = 40;

#[cfg(test)]
mod tests {
    use super::*;

    /// A memfd laid out with the records and the table written, which `read` reads back as it was
    /// written.
    #[test]
    fn round_trip() {
        let layout = Layout::new(5, [3, 0, 9].into_iter()).unwrap();
        // The header, the table of 3 entries, then each record from a multiple of 8.
        assert_eq!(layout.state(), 184..189);
        assert_eq!(layout.item(0), 192..195);
        assert_eq!(layout.item(1), 200..200);
        assert_eq!(layout.item(2), 200..209);
        assert_eq!(layout.len(), 216);
        let mut memfd = vec![0; layout.len()];
        memfd[layout.state()].copy_from_slice(b"state");
        // The second item is left out, as one whose record failed to be saved.
        let entries = [
            Entry {
                id: 7,
                record: layout.item(0),
                fds: 2..3,
            },
            Entry {
                id: u64::MAX,
                record: layout.item(2),
                fds: 3..5,
            },
        ];
        layout.write(&mut memfd, 2, &entries);

        let contents = read(&memfd).unwrap();
        assert_eq!(&memfd[contents.state], b"state");
        assert_eq!(contents.state_fds, 2);
        assert_eq!(contents.entries, entries);
    }

    #[test]
    fn empty() {
        let layout = Layout::new(0, [].into_iter()).unwrap();
        let mut memfd = vec![0; layout.len()];
        layout.write(&mut memfd, 0, &[]);

        let contents = read(&memfd).unwrap();
        assert!(contents.state.is_empty());
        assert_eq!(contents.state_fds, 0);
        assert!(contents.entries.is_empty());
    }

    #[test]
    fn too_long_for_a_memfd() {
        let error = Layout::new(usize::MAX - 8, [].into_iter()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let error = Layout::new(0, [isize::MAX as usize].into_iter()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    /// A memfd that isn't a hand-over of this format is refused, rather than read wrong: garbage,
    /// one of a later major version or that needs a feature unknown here, and one whose lengths,
    /// offsets or counts are beyond its end.
    #[test]
    fn others_are_refused() {
        let layout = Layout::new(5, [3].into_iter()).unwrap();
        let mut memfd = vec![0; layout.len()];
        let entry = Entry {
            id: 1,
            record: layout.item(0),
            fds: 0..1,
        };
        layout.write(&mut memfd, 0, &[entry]);
        read(&memfd).unwrap();

        let unsupported: [(usize, &[u8]); 2] =
            [(8, &2u16.to_le_bytes()), (12, &1u32.to_le_bytes())];
        for (at, bytes) in unsupported {
            let mut spoiled = memfd.clone();
            spoiled[at..at + bytes.len()].copy_from_slice(bytes);
            assert_eq!(read(&spoiled).unwrap_err().kind(), ErrorKind::Unsupported);
        }

        let malformed: [(usize, &[u8]); 9] = [
            // The magic.
            (0, b"x"),
            // Major version 0.
            (8, &0u16.to_le_bytes()),
            // A header shorter than this version's, and one that isn't a multiple of 8.
            (10, &56u16.to_le_bytes()),
            (10, &65u16.to_le_bytes()),
            // The length of the memfd.
            (16, &1u64.to_le_bytes()),
            // More items than the table has room for.
            (24, &2u64.to_le_bytes()),
            // A record of the state beyond the end.
            (48, &(layout.len() as u64).to_le_bytes()),
            // More file descriptors for the state than there are.
            (56, &2u64.to_le_bytes()),
            // An item with more file descriptors than there are.
            (64 + 32, &2u64.to_le_bytes()),
        ];
        for (at, bytes) in malformed {
            let mut spoiled = memfd.clone();
            spoiled[at..at + bytes.len()].copy_from_slice(bytes);
            let error = read(&spoiled).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData, "at {at}: {error}");
        }
        assert!(read(&memfd[..63]).is_err());
        assert!(read(b"").is_err());
    }
}
