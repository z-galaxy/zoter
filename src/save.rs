//! What can be handed over to the next instance of a service: [`Save`] and [`Restore`].

use std::{
    convert::Infallible,
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

/// What can be handed over to the next instance of a service: a record of bytes, and the file
/// descriptors that go with it.
///
/// [`Restore`] gives it back from them, in the next instance. A protocol implements both for the
/// hand-over of a connection, as zbus does for D-Bus, so that a service hands its connections over
/// as they are, as items of a hand-over.
///
/// The trait takes no generics, and its methods return no `impl Trait`, so that the items of a
/// hand-over can be of different types, as `dyn Save`.
pub trait Save {
    /// The exact length of the record, or `None` for what can't be saved, as the hand-over of a
    /// connection that only goes on in this process: a hand-over leaves it out.
    fn saved_len(&self) -> Option<usize>;

    /// Copy the record into `dest`, which is [`saved_len`](Self::saved_len) bytes long, and can be
    /// anywhere in memory, at any address, such as the place of the record in a memfd that is
    /// mapped into memory.
    ///
    /// `dest` is zeroed. A record that fails to be saved leaves its item out of the hand-over, and
    /// nothing else.
    fn save(&self, dest: &mut [u8]) -> io::Result<()>;

    /// The file descriptors that go with the record, in the order that [`Restore::restore`] takes
    /// them.
    fn fds(&self) -> Vec<BorrowedFd<'_>>;
}

/// What can be restored from the record that [`Save::save`] copied, and the file descriptors that
/// [`Save::fds`] gave, in the next instance of a service.
pub trait Restore: Sized {
    /// The error of a record, or of file descriptors, that nothing can be restored from.
    type Error;

    /// Restore from `record`, exactly the bytes that [`Save::save`] copied, at any address, and
    /// `fds`, in the order that [`Save::fds`] gave them.
    ///
    /// The record goes once this returns, as the next instance reads it in place, in the mapping of
    /// a memfd, so this copies what it needs out of it. Those of `fds` that it doesn't keep close.
    fn restore(record: &[u8], fds: Vec<OwnedFd>) -> Result<Self, Self::Error>;
}

/// A record of these bytes, with no file descriptors.
impl Save for [u8] {
    fn saved_len(&self) -> Option<usize> {
        Some(self.len())
    }

    fn save(&self, dest: &mut [u8]) -> io::Result<()> {
        dest.copy_from_slice(self);

        Ok(())
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        Vec::new()
    }
}

/// A record of these bytes, with no file descriptors.
impl Save for Vec<u8> {
    fn saved_len(&self) -> Option<usize> {
        self.as_slice().saved_len()
    }

    fn save(&self, dest: &mut [u8]) -> io::Result<()> {
        self.as_slice().save(dest)
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        Vec::new()
    }
}

/// This file descriptor, with an empty record.
impl Save for OwnedFd {
    fn saved_len(&self) -> Option<usize> {
        Some(0)
    }

    fn save(&self, _dest: &mut [u8]) -> io::Result<()> {
        Ok(())
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        vec![self.as_fd()]
    }
}

/// This file descriptor, with an empty record.
impl Save for BorrowedFd<'_> {
    fn saved_len(&self) -> Option<usize> {
        Some(0)
    }

    fn save(&self, _dest: &mut [u8]) -> io::Result<()> {
        Ok(())
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        vec![self.as_fd()]
    }
}

impl<T> Save for &T
where
    T: Save + ?Sized,
{
    fn saved_len(&self) -> Option<usize> {
        (**self).saved_len()
    }

    fn save(&self, dest: &mut [u8]) -> io::Result<()> {
        (**self).save(dest)
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        (**self).fds()
    }
}

impl<T> Save for Box<T>
where
    T: Save + ?Sized,
{
    fn saved_len(&self) -> Option<usize> {
        (**self).saved_len()
    }

    fn save(&self, dest: &mut [u8]) -> io::Result<()> {
        (**self).save(dest)
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        (**self).fds()
    }
}

/// The bytes of a record that has no file descriptors.
///
/// It fails with [`io::ErrorKind::InvalidData`] if there are any, and closes them.
impl Restore for Vec<u8> {
    type Error = io::Error;

    fn restore(record: &[u8], fds: Vec<OwnedFd>) -> io::Result<Self> {
        if !fds.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "a record of bytes alone came with {} file descriptors",
                    fds.len()
                ),
            ));
        }

        Ok(record.to_vec())
    }
}

/// The file descriptor of an empty record.
///
/// It fails with [`io::ErrorKind::InvalidData`] for a record that isn't empty, or with other than
/// one file descriptor, and closes those that came.
impl Restore for OwnedFd {
    type Error = io::Error;

    fn restore(record: &[u8], fds: Vec<OwnedFd>) -> io::Result<Self> {
        let Ok([fd]) = <[OwnedFd; 1]>::try_from(fds) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a file descriptor came with other than one",
            ));
        };
        if !record.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a file descriptor came with a record",
            ));
        }

        Ok(fd)
    }
}

/// The bytes of the record, and the file descriptors, as they are.
impl Restore for (Vec<u8>, Vec<OwnedFd>) {
    type Error = Infallible;

    fn restore(record: &[u8], fds: Vec<OwnedFd>) -> Result<Self, Infallible> {
        Ok((record.to_vec(), fds))
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::test_utils::pipe;

    #[test]
    fn bytes() {
        let bytes = b"a record".to_vec();
        let mut dest = vec![0; bytes.saved_len().unwrap()];
        bytes.save(&mut dest).unwrap();
        assert_eq!(dest, bytes);
        assert!(bytes.fds().is_empty());

        assert_eq!(Vec::<u8>::restore(&dest, Vec::new()).unwrap(), bytes);
        let (_, write) = pipe();
        let error = Vec::<u8>::restore(&dest, vec![write]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn fd() {
        let (read, write) = pipe();
        assert_eq!(write.saved_len(), Some(0));
        let fds = write.fds();
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].as_raw_fd(), write.as_raw_fd());
        assert_eq!(write.as_fd().fds()[0].as_raw_fd(), write.as_raw_fd());

        let restored = OwnedFd::restore(&[], vec![write]).unwrap();
        let error = OwnedFd::restore(b"x", vec![restored]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = OwnedFd::restore(&[], Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = OwnedFd::restore(&[], vec![read, pipe().1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// Items of different types go together, as `dyn Save`, whether borrowed or owned.
    #[test]
    fn dyn_items() {
        let (_, write) = pipe();
        let bytes = vec![1, 2, 3];
        let items: Vec<Box<dyn Save + '_>> = vec![Box::new(&bytes), Box::new(write.as_fd())];
        let lens: Vec<_> = items.iter().map(|item| item.saved_len()).collect();
        assert_eq!(lens, [Some(3), Some(0)]);
        let fds: Vec<_> = items.iter().map(|item| item.fds().len()).collect();
        assert_eq!(fds, [0, 1]);
    }

    #[test]
    fn raw() {
        let (_, write) = pipe();
        let (record, fds) = <(Vec<u8>, Vec<OwnedFd>)>::restore(b"raw", vec![write]).unwrap();
        assert_eq!(record, b"raw");
        assert_eq!(fds.len(), 1);
    }
}
