# Changelog

## [0.2.1](https://github.com/kayfgit/browser/compare/v0.2.0...v0.2.1) (2026-09-30)


### Bug Fixes

* :freeze stops the web engine instead of only suspending pages ([5d4bbce](https://github.com/kayfgit/browser/commit/5d4bbcea3cd8563fa64f229dcd710ff6facbe839))
* closing a split focuses the pane you were in before, not the first one ([7ef0a68](https://github.com/kayfgit/browser/commit/7ef0a68d3a682493f21c547af0f9d90f15f3d1eb))
* hint-clicking a copy button actually copies ([fbc993f](https://github.com/kayfgit/browser/commit/fbc993fff54c9f849cf8b07e52cf41a8805057c9))
* leave passthrough and insert from inside iframes; drop the dead keyboard hook ([bfb5f3a](https://github.com/kayfgit/browser/commit/bfb5f3a36789511eb71620df1a348b3be9e76913))
* typing in a scrolled-up terminal jumps back to the prompt ([f60c1f0](https://github.com/kayfgit/browser/commit/f60c1f0e4dd8a9ba12d4f10289afee8886482b2c))


### Performance

* cut the browser's idle memory from about 160 MB to 22 MB ([8708264](https://github.com/kayfgit/browser/commit/8708264f93f2e46675d897912f92fec6edceb10f))

## [0.2.0](https://github.com/kayfgit/browser/compare/v0.1.0...v0.2.0) (2026-09-29)


### Features

* add :news to show what changed in each release ([4d8f9e9](https://github.com/kayfgit/browser/commit/4d8f9e910ca968ff8a51af2dfb80591b7955fc5d))
* leave the :ai field with Ctrl+S ([b40c9e9](https://github.com/kayfgit/browser/commit/b40c9e949ed955903df9fe9b4a2ac00c10441b8b))
* render markdown in :ai replies ([d664925](https://github.com/kayfgit/browser/commit/d66492553a5e32bfdc0681f8730c6847147c2806))


### Bug Fixes

* keep the active profile when restoring default settings ([0951462](https://github.com/kayfgit/browser/commit/09514623b37876d7a3e514fcb67229c69ecbd92f))
* no stray spaces around links and code in read mode ([fe12ba1](https://github.com/kayfgit/browser/commit/fe12ba147077e9ead3c57191c5c04bd0f213067d))
* stop aliases from replacing built-in commands ([fe492dc](https://github.com/kayfgit/browser/commit/fe492dc17d90a28f6726ae910a73c86340ad765d))

## 0.1.0 (2026-09-29)


### Features

* build MSI and PowerShell installers for each release ([ca53148](https://github.com/kayfgit/browser/commit/ca531486d8016b6f8e3b4a60400878a8a2d72867))
* installable releases (docs, CI, release automation, MSI installer) ([cdaeca2](https://github.com/kayfgit/browser/commit/cdaeca22576c89a9eb0a452c79f30f860fd15960))


### Bug Fixes

* default the terminal to PowerShell instead of nushell ([8cbbbed](https://github.com/kayfgit/browser/commit/8cbbbedc462262d2e957764f2436861527d9add3))
* hand keys a page grabbed in Normal mode back from the page itself ([04336e4](https://github.com/kayfgit/browser/commit/04336e44a51ad5c4ccb69f525124014117082e2e))
* keep the browser profile when uninstalling ([0be6e3e](https://github.com/kayfgit/browser/commit/0be6e3eef687d069a6249494498f400e52c6f29e))
* make release builds self-contained and installable ([4ce2f7e](https://github.com/kayfgit/browser/commit/4ce2f7e03e137715406f081387a491dca38700c7))
* return the keyboard to the browser after clicking a page control ([1dc15be](https://github.com/kayfgit/browser/commit/1dc15be4ef1d5b6bb56b4bc352150962dffa5c57))
* use an HKCU key path for the MSI's Start Menu shortcut ([1a457a2](https://github.com/kayfgit/browser/commit/1a457a23746beb92bd506b96d7eaf255e41a52e8))
