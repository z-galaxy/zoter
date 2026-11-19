# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.2.0 - 2026-11-19

### ✨ Added
- Tell systemd in Store::take_over that the service has started.
- Add testing::store and testing::Manager::barrier, for tests in place of Store::new and Notifier.

### 💥 Breaking
- Return zoter::Error wherever the API returned io::Error, Save::save included.
- Fail in Store::from_env with Error::NoManager without systemd, rather than return None.
- Create a Store only where systemd keeps a usable store, and remove Store::max.
- Make Store::from_env take &Passed and require systemd socket activation.
- Hand over through Store::hand_over, and remove Store::handover and Handover.
- Store and take over a hand-over whole, or fail, removing Store::set_fd_limit.
- Remove LeftOut, Stored::left_out and Restored::left_out, as nothing is left out any more.
- Remove Notifier, Store::notifier, Store::new and testing::Manager::notifier.

## 0.1.0 - 2026-11-11

### ✨ Added
- Give the notifications that the fake manager got and nobody waited for.
- Count the file descriptors of a hand-over in the fake manager.
- Add Store, to hand a service over to its next instance.
- Add testing::Manager, a fake service manager for tests.
- Add Notifier, the notification socket of the service manager.
- Add Passed, the file descriptors that systemd passes to a service.
- Add Save and Restore, for what a service hands over.

### 👷 CI
- Run the benchmarks on CodSpeed.

### 📝 Documentation
- Show the logo in the README and API docs.
- Add a Newton's cradle logo.
- Tell in the README how a service restarts with zoter.

### ✅ Testing
- Benchmark how a hand-over is stored and taken over.
- Test a restart of a service end to end.
