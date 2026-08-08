# fast-down

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![codecov](https://codecov.io/gh/fast-down/core/branch/main/graph/badge.svg)](https://codecov.io/gh/fast-down/core)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

`fast-down` **Fastest** concurrent downloader!

- fast-steal: [![Latest version](https://img.shields.io/crates/v/fast-steal.svg)](https://crates.io/crates/fast-steal) [![Documentation](https://docs.rs/fast-steal/badge.svg)](https://docs.rs/fast-steal)
- fast-pull: [![Latest version](https://img.shields.io/crates/v/fast-pull.svg)](https://crates.io/crates/fast-pull) [![Documentation](https://docs.rs/fast-pull/badge.svg)](https://docs.rs/fast-pull)
- fast-down: [![Latest version](https://img.shields.io/crates/v/fast-down.svg)](https://crates.io/crates/fast-down) [![Documentation](https://docs.rs/fast-down/badge.svg)](https://docs.rs/fast-down)
- fast-down-api: [![Latest version](https://img.shields.io/crates/v/fast-down-api.svg)](https://crates.io/crates/fast-down-api) [![Documentation](https://docs.rs/fast-down-api/badge.svg)](https://docs.rs/fast-down-api)

**[Official Website (Simplified Chinese)](https://fd.s121.top/)**

## Features

1. **⚡️ Fastest Download**  
   We created [fast-steal](https://github.com/fast-down/fast-steal) With optimized Work Stealing, **1.43 x faster** than NDM.
2. **🔄 File consistency**  
   Switching Wi-Fi, Turn Off Wi-Fi, Switch proxies. **We guarantee the consistency**.
3. **⛓️‍💥 Resuming Downloads**  
   You can **interrupt** at any time, and **resume downloading** after.
4. **⛓️‍💥 Incremental Downloads**  
   1000 more lines server logs? Don't worry, we **only download new lines**.
5. **💰 Free and open-source**  
   The code stays free and open-source. Thanks to [share121](https://github.com/share121), [Cyan](https://github.com/CyanChanges) and other fast-down contributors.
6. **💻 Cross platform**

   | Arch   | Windows       | Linux         | macOS            |
   | ------ | ------------- | ------------- | ---------------- |
   | 64 bit | [Download][1] | [Download][1] | [Download][1]    |
   | 32 bit | [Download][1] | [Download][1] | ❌ Not Supported |
   | Arm64  | [Download][1] | [Download][1] | [Download][1]    |

[1]: https://fd.s121.top/#install
