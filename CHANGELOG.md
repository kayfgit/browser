# Changelog

## [0.7.0](https://github.com/kayfgit/browser/compare/v0.6.0...v0.7.0) (2026-10-10)


### Features

* block ad and tracker requests in Servo pages ([25d5bad](https://github.com/kayfgit/browser/commit/25d5bad1ed60dceb14b2894a5e1c0fcf85442f44))


### Bug Fixes

* hints click YouTube's skip-ad button and captcha checkboxes ([ca1c894](https://github.com/kayfgit/browser/commit/ca1c8948dc5967303d607fb02aeec06abc0ea5a5))
* internal pages no longer replace the focused pane of a split ([8e0fd0a](https://github.com/kayfgit/browser/commit/8e0fd0a7cd40290925827c8c58588c0f3e56b544))
* jump to the top with gg instead of a single g ([b67712f](https://github.com/kayfgit/browser/commit/b67712f558dddda1584b3e3fa6f202e85ba0e1e0))
* keep uBlock Origin Lite on and make it the only WebView2 ad blocker ([d86d396](https://github.com/kayfgit/browser/commit/d86d396c37a572c4ad97da66b8692425e6db88e7))
* stop reloading uBlock Origin Lite under loading pages ([c7a2aba](https://github.com/kayfgit/browser/commit/c7a2aba6f3c164976192c921cde06963c9d391fc))

## [0.6.0](https://github.com/kayfgit/browser/compare/v0.4.0...v0.6.0) (2026-10-09)


### Features

* a crashing Servo page only takes down its own pane ([b64fc9a](https://github.com/kayfgit/browser/commit/b64fc9a15775735dc486659354783f85fa76c20a))
* ship Servo as a second engine in releases ([c824442](https://github.com/kayfgit/browser/commit/c82444252d98cb2956bf12d92e363c000aed93d3))
* upgrade the Servo engine to 0.6.0 ([51f0686](https://github.com/kayfgit/browser/commit/51f0686b17b64fd8767d32f8399d6d1b1c9133f6))


### Bug Fixes

* drop the PowerShell installer from releases ([47c155a](https://github.com/kayfgit/browser/commit/47c155a1be042bad1fdef5ac6f7fdb1df2791763))
* GitHub and other modern sites work in Servo ([1ea55df](https://github.com/kayfgit/browser/commit/1ea55dfe192915e26e18cc82eed7555ada793d4a))
* hints on buttons and keys a page hands back now work in Servo panes ([c6b47cc](https://github.com/kayfgit/browser/commit/c6b47ccd7a583bbb464b8c4848de9f71202fb818))
* release builds link the C runtime Servo needs, and ship it ([6946cb9](https://github.com/kayfgit/browser/commit/6946cb9d5543cc6d33c4f1781742ea98d2306207))
* Servo's page processes no longer outlive the browser ([7c0c0d8](https://github.com/kayfgit/browser/commit/7c0c0d8826258c90e7aca687bb305a18cf7b2cfc))

## [0.4.0](https://github.com/kayfgit/browser/compare/v0.3.0...v0.4.0) (2026-10-02)


### Features

* install per user and update automatically on quit ([a599b31](https://github.com/kayfgit/browser/commit/a599b315331567d5a9b34bdd86f700a07fe10f25))


### Bug Fixes

* find the old per-machine copy through Windows Installer, not by name ([f356e8d](https://github.com/kayfgit/browser/commit/f356e8d1817e2170d844f01b64e49a12d2fbf807))
* remove the downloaded installer after an update ([2debc4a](https://github.com/kayfgit/browser/commit/2debc4a79dc329ba69f2b5a3926728eaa2b8b864))

## [0.3.0](https://github.com/kayfgit/browser/compare/v0.2.1...v0.3.0) (2026-10-02)


### ⚠ BREAKING CHANGES

* :q, :wq and :x no longer quit. Use :quit (or :leave, :l); save first with :w.

### Features

* :update checks for a new version and installs it ([9647b6b](https://github.com/kayfgit/browser/commit/9647b6b544f2b75071b67dc664d4e3ea6542fad0))
* 13,000+ bangs from Kagi's list, and your own with :bang ([a4b1f8d](https://github.com/kayfgit/browser/commit/a4b1f8da9c562c304b7b6ff82799b8080d8659b6))
* inspect a page and view its source ([206ad3d](https://github.com/kayfgit/browser/commit/206ad3ddef0994c82ee9c2e33e71b20e2c4529ad))
* Kagi's list is the only bang set; :unbang switches bangs off, :resetbangs restores ([a65ab34](https://github.com/kayfgit/browser/commit/a65ab34fe58f883792e50a473612675f67875682))
* quit with :quit, :leave or :l; vim's :q no longer closes the browser ([dd2886e](https://github.com/kayfgit/browser/commit/dd2886e78feefe25d86406c539cadc6bfc03d896))
* undo and redo layout changes with U and R ([1d7db58](https://github.com/kayfgit/browser/commit/1d7db58854446a3242a8de1e1d431290257a4530))


### Bug Fixes

* a view-source tab keeps its address, so undoing its close reopens the source ([904c75f](https://github.com/kayfgit/browser/commit/904c75f3a5bac1ec7bc0450f9f54beda2cf12586))


### Performance

* read fallback fonts one glyph at a time instead of parsing whole faces ([1085da4](https://github.com/kayfgit/browser/commit/1085da4c892efd1bc8269463d6fd24ff35a7fe37))

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
