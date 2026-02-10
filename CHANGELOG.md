# Changelog

All notable changes to this project will be documented in this file.


## [v1.2.2] - 2026-02-10


### Bug Fixes

- Zed compatibility — cap max_tokens, strip temperature for thinking models, add Zed setup ([f5564ba](https://github.com/skyline69/agcp/commit/f5564ba731fa48fea211d955dd9963cf21e26b49)) — Ö. Efe D.
- Formatting ([1e1a836](https://github.com/skyline69/agcp/commit/1e1a8363efcdc7e39821beea4379aaae48b6d593)) — Ö. Efe D.


### Miscellaneous

- Update to v1.2.2 ([76c3304](https://github.com/skyline69/agcp/commit/76c33042f75e69067b23f0323d4c576ab1ce8a67)) — Ö. Efe D.


## [v1.2.1] - 2026-02-10


### Bug Fixes

- Map claude-3-5-haiku to gemini-3-flash to avoid 404 on Cloud Code ([0327047](https://github.com/skyline69/agcp/commit/0327047325b600f7094702a131edf3ceed005e51)) — Ö. Efe D.


### Miscellaneous

- Update version ([2ccc2a9](https://github.com/skyline69/agcp/commit/2ccc2a9644895636d088ff606c4089113d0a9894)) — Ö. Efe D.


## [v1.2.0] - 2026-02-09


### Bug Fixes

- Update help screen with correct tab count and missing shortcuts ([0fb4379](https://github.com/skyline69/agcp/commit/0fb43797d99234cf2a68bab47353eca9d15d0087)) — Ö. Efe D.
- Cache-bust style.css for GitHub Pages ([4c755b4](https://github.com/skyline69/agcp/commit/4c755b4e5cba8198542a10ec5418a220ecdf5831)) — Ö. Efe D.
- Use first snapshot as chart time origin instead of quota period start ([caf95bb](https://github.com/skyline69/agcp/commit/caf95bb054a394fe3f68913f85cd38b558da4716)) — Ö. Efe D.
- Prevent chart line from dropping on server restart ([e13469e](https://github.com/skyline69/agcp/commit/e13469e40acfa95631c1b8f0031bde509ed434e8)) — Ö. Efe D.
- Only poll /stats on Usage/Overview tabs, enforce monotonic chart lines ([aeec02e](https://github.com/skyline69/agcp/commit/aeec02eda4acff79942347cbf6cbfc4c6541e2b6)) — Ö. Efe D.
- Sort models by usage in chart legend and summary panel ([25696d5](https://github.com/skyline69/agcp/commit/25696d55248f1d2a47ffc3ffb6cc8d762788ef89)) — Ö. Efe D.
- Correct chart restart detection for disappearing models ([93692f6](https://github.com/skyline69/agcp/commit/93692f63ec5a3cfd7f4e84f9b54bb3ac3652b4cb)) — Ö. Efe D.
- Uptime stuck at 00:00:00 due to /stats polling flooding logs ([e12faf0](https://github.com/skyline69/agcp/commit/e12faf056aa074826fa986c1d982d2788fb0a8c3)) — Ö. Efe D.
- OpenCode and Crush not detected on macOS in setup command ([35ff410](https://github.com/skyline69/agcp/commit/35ff410fb59198a9509c27742c39f6ed2fc21565)) — Ö. Efe D.
- Crush 400 INVALID_ARGUMENT due to excessive maxOutputTokens ([721f909](https://github.com/skyline69/agcp/commit/721f909e9eaf65e3e9297f35968706bb55281839)) — Ö. Efe D.
- Formatting ([9171bae](https://github.com/skyline69/agcp/commit/9171bae1e7193ce68ed8f622c78491676b8033e8)) — Ö. Efe D.


### Documentation

- Add 'Works with' logo strip, 'Why would I need this?' section, and ratatui footer mention ([d4900b7](https://github.com/skyline69/agcp/commit/d4900b7316c4ffab3af382d3e28272643da3c947)) — Ö. Efe D.
- Add Usage tab, Crush support, and token tracking to website ([ab61e95](https://github.com/skyline69/agcp/commit/ab61e95d2c179f80469ea53721a42a40b7785b5f)) — Ö. Efe D.


### Features

- Add tool_choice support, token usage tracking, and TUI usage tab ([ec4eaf0](https://github.com/skyline69/agcp/commit/ec4eaf046a2226d90674593ba859631e98a88f4f)) — Ö. Efe D.
- Replace bar chart with multi-dataset time-series line chart in Usage tab ([7ae65f2](https://github.com/skyline69/agcp/commit/7ae65f246bb55e4c2250c2f4072a5a22ca9ded7c)) — Ö. Efe D.
- Cumulative token chart with persistence and auto-reset on quota period ([437f301](https://github.com/skyline69/agcp/commit/437f3018f015a7881a9394ca6f390fd54de467a7)) — Ö. Efe D.
- Add missing tab sections to help overlay in two-column layout ([cb15b3f](https://github.com/skyline69/agcp/commit/cb15b3f8087ef6db36db47f874117f3048d71287)) — Ö. Efe D.


### Miscellaneous

- Format code ([7fb4f8c](https://github.com/skyline69/agcp/commit/7fb4f8c2c9b649f1fc2896de3e529465b374b12e)) — Ö. Efe D.
- Update version & update dependencies ([48d12c3](https://github.com/skyline69/agcp/commit/48d12c395ce9a952df68ba3da766b9dab7ba3f33)) — Ö. Efe D.


### Performance

- Reduce memory allocations across hot paths ([64412ed](https://github.com/skyline69/agcp/commit/64412ed497a830dd969a2a6eb52f51af1389d6d6)) — Ö. Efe D.
- Increase chart update rate to 1s for smoother lines ([715f75c](https://github.com/skyline69/agcp/commit/715f75cb1ea6a1db72845d3cfabc7222fb8840b0)) — Ö. Efe D.
- Only record token snapshots when values change ([1816137](https://github.com/skyline69/agcp/commit/18161379225fee8c6d4fd405f27ff08bcfa8cc7c)) — Ö. Efe D.


## [v1.1.0] - 2026-02-08


### Bug Fixes

- Exclude auto-generated changelog commits from git-cliff output ([8e3a168](https://github.com/skyline69/agcp/commit/8e3a1687afaede4a91f44a1244f32240db2be75e)) — Ö. Efe D.
- Subscription tier detection, TUI log path on macOS, and lazy startup performance ([ee42ee4](https://github.com/skyline69/agcp/commit/ee42ee4eb32d346ba9f4fbefd411484db776e6d9)) — Ö. Efe D.
- Per-account quota fetching and display in TUI ([3a3c914](https://github.com/skyline69/agcp/commit/3a3c91437aeebc52a8f052a20901bf2d2dcd3ed3)) — Ö. Efe D.
- Daemon restart from Config tab using correct launch method ([e7e95f7](https://github.com/skyline69/agcp/commit/e7e95f7bceac975804bc7139975af0676a822501)) — Ö. Efe D.
- Update model list to match available models, add GPT-OSS 120B ([121b050](https://github.com/skyline69/agcp/commit/121b050b2790c4900e4f8f7ba831729af9fd9647)) — Ö. Efe D.
- Use PID file check instead of TCP probe for server status ([37e139a](https://github.com/skyline69/agcp/commit/37e139ad73a8d17b42ebe1e78f236916b92675bb)) — Ö. Efe D.
- Unused variable on Windows ([09cc712](https://github.com/skyline69/agcp/commit/09cc712b3812019b97cca52ece78ecdb321befaf)) — Ö. Efe D.
- Correct binary size to ~3MB ([acb7e79](https://github.com/skyline69/agcp/commit/acb7e797f5920b2acde54b0cde5468fcee79a8ce)) — Ö. Efe D.


### Features

- Add log filtering, search, and account filter to TUI Logs tab ([9fdf332](https://github.com/skyline69/agcp/commit/9fdf332b36404260148c3ce23ea7c3a36850fe27)) — Ö. Efe D.
- Add tooltips to Config tab fields ([61995b1](https://github.com/skyline69/agcp/commit/61995b177f01f65f13ad62ddeee9f8163ff2db40)) — Ö. Efe D.
- Add search and sort to Accounts tab ([60ed1ca](https://github.com/skyline69/agcp/commit/60ed1ca17ce898b40c558e622b3e97a726f91e04)) — Ö. Efe D.
- Add Mappings tab with presets, glob rules, and background task model ([3f25f8a](https://github.com/skyline69/agcp/commit/3f25f8a2642867cc9dcc0d346fce628401d2ebf4)) — Ö. Efe D.
- Add daemon start/stop/restart controls to Overview tab, update docs ([6e6e8cb](https://github.com/skyline69/agcp/commit/6e6e8cb0d9bb28f72cd3b7c3e692a2f5a908a36b)) — Ö. Efe D.


### Miscellaneous

- Update version ([7efd77b](https://github.com/skyline69/agcp/commit/7efd77b2c5c1062d5ed7361acfd336590881a41e)) — Ö. Efe D.
- Format code ([76eac4d](https://github.com/skyline69/agcp/commit/76eac4da933e40e33b369952122586e57b8a4719)) — Ö. Efe D.
- Format code ([371f24d](https://github.com/skyline69/agcp/commit/371f24de37fcbe90a16c5de484b392da5cb8f568)) — Ö. Efe D.


## [v1.0.1] - 2026-02-07


### Bug Fixes

- Gate unix-only imports and functions with cfg(unix) for Windows compatibility ([27f62f0](https://github.com/skyline69/agcp/commit/27f62f0b93fb3f77a71a135aa103950cecc39579)) — Ö. Efe D.
- Remove redundant mv in deb build step ([ce7d052](https://github.com/skyline69/agcp/commit/ce7d052279b5c09cd7fd416fe5a4da5942e5b42e)) — Ö. Efe D.
- Use --unreleased for release changelog generation ([271289f](https://github.com/skyline69/agcp/commit/271289ff5a5e98aff348620c672fed93c427610a)) — Ö. Efe D.
- Write PKGBUILD to workspace instead of /tmp for Docker compatibility ([0f4b9da](https://github.com/skyline69/agcp/commit/0f4b9da733ec6bebf4174bc9bf7b663748ae7f06)) — Ö. Efe D.


### Miscellaneous

- Auto-update CHANGELOG.md on release ([849ece2](https://github.com/skyline69/agcp/commit/849ece2c1dfff86c90b1e625138d74c4529dc1b7)) — Ö. Efe D.


## [v1.0.0] - 2026-02-07


### Bug Fixes

- Read version from Cargo.toml in Nix flake ([c05e078](https://github.com/skyline69/agcp/commit/c05e078eaf876d52d9b36c7a4cdd745e52eff459)) — Ö. Efe D.
- Cursor pointer, dollar sign prompt, and copy button visibility ([246a7fd](https://github.com/skyline69/agcp/commit/246a7fd757871ef792feb6efd1e07ea48c12ecd4)) — Ö. Efe D.


### Documentation

- Add crates.io badge to README ([fe3e80c](https://github.com/skyline69/agcp/commit/fe3e80c54a2a731abc033b1118dba01b4af65c4f)) — Ö. Efe D.
- Add Homebrew install instructions to README ([fcc9c0a](https://github.com/skyline69/agcp/commit/fcc9c0af6fa574066d130d494df5ab18d8126f18)) — Ö. Efe D.
- Use custom domain for APT repo URL ([00dc509](https://github.com/skyline69/agcp/commit/00dc5097dee179e3933d4156906adbf5637d2e02)) — Ö. Efe D.
- Add tabbed installation section to website ([80bc0e8](https://github.com/skyline69/agcp/commit/80bc0e8013a8b5560466dec3cef7326e7f6ebeed)) — Ö. Efe D.
- Add copy buttons, cursor pointer, and unselectable prompt to terminal blocks ([a217b63](https://github.com/skyline69/agcp/commit/a217b634dcfd8b2e7004c41ef38d3faa0c18f2dd)) — Ö. Efe D.
- Add shimmer skeleton loader for lazy-loaded images ([c41d3fe](https://github.com/skyline69/agcp/commit/c41d3fe5247306029b211f248f26020ac0c2ed6c)) — Ö. Efe D.
- Fade scroll arrow on scroll down ([f42adde](https://github.com/skyline69/agcp/commit/f42adde30a86e886a9d4c9224642a2620bda5fe6)) — Ö. Efe D.


### Features

- Initial release (v1.0.0) ([6a18062](https://github.com/skyline69/agcp/commit/6a180625a02ab3b58f91ccf79aaba4432b54f735)) — Ö. Efe D.


### Miscellaneous

- Add macOS and Windows to CI matrix ([3eddf3e](https://github.com/skyline69/agcp/commit/3eddf3e0fa7176fc50859fa598867011982c8567)) — Ö. Efe D.
- Add Homebrew tap auto-update workflow and formula ([afeb61c](https://github.com/skyline69/agcp/commit/afeb61c6412c6f0cb974d3f5b5009f432332bd2a)) — Ö. Efe D.
- Add APT repository support with .deb packaging ([057996b](https://github.com/skyline69/agcp/commit/057996b83d894748d7bedb64eb5cd80137426258)) — Ö. Efe D.
- Add RPM repo support and Nix flake ([a29e511](https://github.com/skyline69/agcp/commit/a29e5110491b07495cc1fe166ba2974b8a06ca8b)) — Ö. Efe D.

<!-- generated by git-cliff -->
