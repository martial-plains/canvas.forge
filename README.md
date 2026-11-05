# forge-plugin-canvas


## Install with use_package

Add this to `~/.config/forge/init.rhai`:

```rhai
use_package("canvas", #{
  vc: "git@github.com:martial-plains/canvas.forge.git",
  extensions: ["canvas"],
  config: #{ vault: "~/Notes" },
});
```

Change `vault` to match your notes folder. Forge asks before running the
package build command the first time.

The plugin asks for no directory access by default, so file cards show their
path but not the note's text. To show the text, grant it your vault:

```rhai
forge.plugin.grant("canvas", #{ directories: ["~/Notes"] });
```

## Building

This plugin is built against the Forge plugin SDK (`forge-plugin-sdk`),
taken from the Forge repository named in `Cargo.toml` (change the `git =`
URL if Forge lives elsewhere).

    rustup target add wasm32-wasip2
    cargo build --release --target wasm32-wasip2

Copy the resulting `.wasm` to `~/.config/forge/plugins/`.

## License

GPL-3.0-or-later. See the license file in this directory.
