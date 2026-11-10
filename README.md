# Capipimon

Become a Pokémon and fight other players in Minecraft. Capipimon is a free, fan-made PvP mod for
[Horizon](https://github.com/horizon-rust/horizon), with move kits, guard and dodge, and hundreds of creatures drawn
from community Cobblemon-format models.

## What you need

- The [Horizon launcher](https://github.com/horizon-rust/horizon).
- Minecraft 26.3, which needs your own copy of the game.
- Time for the first build: usually 10 to 30 minutes, depending on your PC. Later launches don't rebuild.

## Install

Capipimon is two listings on the Horizon launcher's Mods page. **You need both, in the same instance.** One alone
won't work.

1. Install **Capipimon** (`capipimon_models`), the game code. It's a native mod, so the launcher asks you to trust it.
   Native mods run with the same access to your computer as the game.
2. Install **Capipimon Content** (`capipimon`), the textures and creature data.
3. Build and play. The launcher downloads what it needs and compiles the mod on your PC.

From a terminal, the same steps are:

```
horizon-launcher mods install capipimon_models --source horizon
horizon-launcher mods install capipimon --source horizon
horizon-launcher native-build --trust-native-code
```

Keep both listings on the same version. A server refuses players whose content doesn't match its own.

## Playing

Capipimon is multiplayer, and no server address comes with it. You'll need the address of a Capipimon server from
whoever runs it, and that server may have a whitelist.

This is the client: it draws the creatures, effects and HUD and sends your controls. The game rules run on the
Capipimon server, which isn't public, so single player and LAN worlds don't do anything with this mod alone.

## What's in this repository

| Folder | Listing | What it is |
| --- | --- | --- |
| `native/` | Capipimon | The Rust source for the client. |
| `content/` | Both | Entity types, textures, language files, the model library and the licence texts. |
| `tools/package/` | Both | `build_packages.py`, which builds the listing files from this repository. |

Capipimon is built against Horizon commit `66f3981f522b24c250b21523a70080965b940424`, pinned in
`native/horizon-mixin.toml`. The launcher fetches that version when it builds the mod.

To build the listing files yourself, with Python 3.10 or newer: `python tools/package/build_packages.py --out <folder>`.

## Credits and licences

Capipimon's own code is licensed under GPL-3.0-only. See [LICENSE](LICENSE).

The models, animations and textures come from Cobblemon and many community add-on packs. Each keeps its own licence
and credit, listed in [CREDITS.md](CREDITS.md), with the licence texts in `content/licences/`. Several of them are
non-commercial, so Capipimon must stay free: it isn't sold, paywalled or monetised.

## Disclaimer

Capipimon is an unofficial fan project. It is not affiliated with or endorsed by Nintendo, The Pokémon Company, Game
Freak or Phase Connect. Pokémon and all related names are trademarks of their respective owners. Minecraft is a
trademark of Mojang, and Capipimon is not approved by or associated with Mojang or Microsoft.
