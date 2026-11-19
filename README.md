# Capipimon

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

Become a Pokémon and fight other players in Minecraft. Capipimon is a free, fan-made PvP game with move kits, guard
and dodge, and hundreds of creatures drawn from community Cobblemon-format models. Our mascot is the capipi, a pink
plush bunny.

You need a Microsoft account that owns Minecraft: Java Edition, and a Windows PC (64-bit).

## Get Capipimon

1. Go to the [latest release](https://github.com/Onii133/capipimon/releases/latest) and download
   **Capipimon.Launcher.Setup.exe**.
2. Run it. It installs for your Windows user only, no administrator rights needed.
3. Sign in with the Microsoft account that owns Minecraft: Java Edition. You enter a short code at microsoft.com/link,
   so we never see your password. Capipimon's Microsoft sign-in is approved by Mojang.
4. The launcher downloads Capipimon and Minecraft's own game files, then you're ready to play. Updates install
   themselves the next time you open the launcher.

Windows may show "Windows protected your PC" the first time, because the launcher isn't code-signed yet. Click
**More info**, then **Run anyway**. Code signing is on the way (see below).

## Is it safe?

- Every release comes with `SHA256SUMS`, the checksums of each file, and `SHA256SUMS.sig`, a signature made with
  Capipimon's release key. The launcher checks every download against them and refuses anything that doesn't match.
- Minecraft's game files come straight from Mojang's servers for your account. We don't redistribute them.
- Coming soon: VirusTotal scan links on each release, and code signing as **Capippalist Trade Guild**, so Windows
  knows who made the launcher.

## Join the server

For now, the Capipimon server is at:

```
chords-render.tun.ply.gg:39890
```

This address is temporary and only for playing before the stream launch. It will change, and the new one will be
posted here.

In the game, open **Multiplayer**, add a server with that address, and join.

The server has a whitelist, so ask Onii to add your Minecraft name before you try. It's also being updated to the
newest Capipimon right now, so if you can't get in, it may be restarting: try again a little later.

Capipimon is multiplayer only. The game rules run on the server, so single player and LAN worlds don't do anything.

## Who runs Capipimon

Capipimon is made and run by **Onii**, not by Mojang or Microsoft. Onii is responsible for the game, the launcher and
the servers.

Contact: **Onii@capippalisttradeguild.com**

## Your data

- Capipimon Launcher collects and sends no data: no statistics, crash reports or pings.
- Capipimon's servers keep only what the game needs: your Minecraft name, UUID and Capipimon progress.
- Onii, not Mojang or Microsoft, is responsible for that data. To have yours removed, email the address above.

The full privacy statement and terms of use are in [PRIVACY.md](PRIVACY.md).

## Cost

Capipimon is free. There is nothing to buy: no purchases or paid items. In-game loot boxes give cosmetics only and
can't be bought with real money.

## What's in this repository

This repository holds the Capipimon client: the part that runs on your PC and draws the creatures, effects and HUD.
The server isn't public.

| Folder | What it is |
| --- | --- |
| `native/` | The Rust source for the client. |
| `content/` | Entity types, textures, language files, the model library and the licence texts. |
| `tools/package/` | `build_packages.py`, which builds the client package from this repository. |

## Credits and licences

Capipimon's own code is licensed under GPL-3.0-only. See [LICENSE](LICENSE).

Capipimon's engine and launcher started from MIT-licensed versions of Horizon (engine commit 66f3981, launcher
0.2.68). Thank you to its authors.

The models, animations and textures come from Cobblemon and many community add-on packs. Each keeps its own licence
and credit, listed in [CREDITS.md](CREDITS.md), with the licence texts in `content/licences/`. Several of them are
non-commercial, so Capipimon must stay free: it isn't sold, paywalled or monetised.

## Disclaimer

Capipimon is an unofficial fan project. It is not affiliated with or endorsed by Nintendo, The Pokémon Company, Game
Freak or Phase Connect. Pokémon and all related names are trademarks of their respective owners. Minecraft is a
trademark of Mojang Synergies AB, and Capipimon is not approved by or associated with Mojang or Microsoft.
