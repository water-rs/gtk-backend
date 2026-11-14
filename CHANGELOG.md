# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/water-rs/gtk-backend/compare/v0.2.1...v0.3.0) - 2026-11-14

### Added

- *(menu)* arm and show menu command shortcuts
- *(app)* run the termination hooks when the session ends
- *(app)* end the run through the termination machine
- *(window)* re-apply the window background when it changes
- *(window)* apply the window style and its changes
- implement window state write-back and maximize for the window API
- *(window)* honour WindowPlacement and Activation (water-rs/waterui#1302) ([#132](https://github.com/water-rs/gtk-backend/pull/132))
- *(app)* run apps with no window and honour the last-window policy ([#131](https://github.com/water-rs/gtk-backend/pull/131))
- *(drag-drop)* [**breaking**] port drag and drop to typed payloads
- *(scroll)* report the content offset through ScrollView::report_offset
- *(input)* bubble unconsumed keys to on_key_press ancestors and submit on Entry activate
- *(anchored-overlay)* write back the placed edge and pin waterui dev
- *(renderer)* deny unaccepted pointer buttons in tap and long-press
- *(gesture)* route the pressing button through GTK gesture observers
- [**breaking**] draw shadows from the declared silhouette
- present AnchoredOverlay in a contract-placed GtkPopover

### Fixed

- satisfy clippy on the window background helper
- follow the framework's shortcut, scroll, surface-wake and cookie APIs
- *(graphics)* fail fast on unimportable dma-buf format sets
- *(graphics)* present GPU surfaces through dma-buf render targets
- consume explicit resources for all GTK application windows
- resolve application icons through resource context
- *(window)* drop test imports the parent module already provides
- *(window)* restore from every state and follow compositor transitions
- *(scroll)* break adjustment closures' cycle and silence cast lint
- compile the shadow widget against gtk 4.14
- keep overlay geometry in GTK pixel units with one checked conversion
- move the popover state into the dismiss closure
- *(webview)* read redirects_enabled through snapshot
- run the WebKitGTK system tests on the process main thread
- [**breaking**] take the executor refresh rate from GDK

### Other

- resolve map-gpu main head in setup-e2e.sh ([#169](https://github.com/water-rs/gtk-backend/pull/169))
- release from main and retire dev ([#166](https://github.com/water-rs/gtk-backend/pull/166))
- *(deps)* point migrated Batch 1 git dependencies to main ([#164](https://github.com/water-rs/gtk-backend/pull/164))
- track the framework, wgpu and waterkit branches and let the lock record the commits
- drop the shaderloom pin the framework revision no longer carries
- Merge remote-tracking branch 'origin/dev' into feat/1340-window-icon
- gate dev on one aggregate check that release pull requests can pass
- require nami 0.12 and drop the nami git pin
- *(deps)* pin vello to the revisions waterui dev requires
- *(deps)* pin waterui to the rebased window-API revision
- pin waterui to the scroll-offset API
- pin waterui to the gesture pointer buttons
- pin waterui to the merged silhouette head
- [**breaking**] migrate to the nami snapshot API
- pin the framework at the merged refresh-rate change

## [0.2.1](https://github.com/water-rs/gtk-backend/compare/v0.2.0...v0.2.1) - 2026-09-21

### Fixed

- *(ci)* install native dependencies before cargo-semver-checks ([#112](https://github.com/water-rs/gtk-backend/pull/112))
- *(ci)* make the MSRV job build with the declared floor ([#110](https://github.com/water-rs/gtk-backend/pull/110))

### Other

- consume the 0.5.0 framework wave from crates.io ([#109](https://github.com/water-rs/gtk-backend/pull/109))
