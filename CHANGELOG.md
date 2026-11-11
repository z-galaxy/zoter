# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
