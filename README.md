# Infinity Viewports

Utilities for forcing antother customizable viewport to render in MSFS2024

## Disclaimer

- It has been years of asking for native support of a feature like this, and although it seems like MSFS wants to add it, we need it now for the DC-10. When the day comes when native support is added it will make this library obsolete
- This will more than likely trigger most antivirus software: Due to the methods used in this library (hooking, writing memory using winapi etc.) it will likely be flagged as a game hack or something similar, hence why its open sourced

## Building

```
cargo build --workspace
cargo test  --workspace
```

## Running test bridge

```
cargo run -p infinity-viewports-testhost -- --bridge target\debug\infinity_viewports_bridge.dll --once        # status + generate logs
cargo run -p infinity-viewports-testhost -- --bridge target\debug\infinity_viewports_bridge.dll --inventory   # RT/DS inventory
cargo run -p infinity-viewports-testhost -- --bridge target\debug\infinity_viewports_bridge.dll --scan-nose-layers nosescan
cargo run -p infinity-viewports-testhost -- --bridge target\debug\infinity_viewports_bridge.dll --teardown    # unhook + unload both dlls
```
