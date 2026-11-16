![CooP Sekiro](media/banner.jpg)

# CooP Sekiro

By **xCløuD** — forked from *Sekiro Together* by **Tihvert**.

Online co-op mod for **Sekiro: Shadows Die Twice** with simple drag-and-drop installation and a dedicated launcher (`coop_sekiro_launcher.exe`). You and your friends (2-6 players) play in the same world, fight the same enemies and bosses, and the session keeps going when someone dies, warps, or hits a loading screen.

Launching Sekiro through Steam starts vanilla unmodded Sekiro; launching through `coop_sekiro_launcher.exe` starts CooP Sekiro with its own separate save file (`S0000.coop.sl2`). Neither clashes.

## Heads up, read this first

**The mod is still a beta.** Back up your saves before playing.

**Only play with people you trust.** Sekiro never got the netcode security patches the other FromSoftware titles got, and this mod runs the game's own online code between players. Co-op stays off until you set a password, and only people with the same password can join you.

This repository is a fork of **Tihvert**'s open-source *Sekiro Together*, now maintained by **xCløuD** cleanly structured without an external `me3` dependency.

## What works

- Simple drag-and-drop install (`coop_sekiro_launcher.exe` + `CoopSekiro\` folder) with separate vanilla and co-op launches.
- 2-6 players, one host and everyone else joins the host (2 players extensively tested; 3+ only in unit tests so far).
- Other players show up as real player characters with name, ping and their gear.
- Enemies and bosses synced (carries the game's own online netcode over CooP Sekiro's connection).
- Bosses, idols, shortcuts and NPC quest progress synced.
- Boss rules: if you die in a boss fight you wait or spectate until the boss dies or everyone's dead. Spectate camera can switch between alive players.
- Resting respawns enemies for everyone.
- XP, sen, drops, boss rewards and key items shared (configurable).
- Enemy and boss scaling for co-op, optional death debuff.
- Deathblow lock so two people can't deathblow the same enemy.
- Party HP bars.
- CooP Sekiro Settings page inside the game's own Options menu.
- Co-op uses its own save file (`S0000.coop.sl2`), automatically copied from your vanilla save the first time without touching `S0000.sl2`. Auto backups every 10 min.

## What's broken / not done (inherited from upstream, have attempted to get these fixed but needs further testing before I can cross these off)

- Enemies or bosses can desync now and then.
- Levers, lifts and breakables are not synced; doors only partially.
- Resurrect / spectate camera edge cases are still being hardened.
- Story phases, Dragonrot, Bell Demon and cutscenes aren't handled specially yet.
- Host stutter spikes can occur during area transitions / heavy combat; 3-6 players not tested live yet.

## How to install

You need:

- Sekiro on **Steam, version 1.06** (normal unmodded game). Only the Steam version is supported.
- Windows, or Linux / Steam Deck with Proton (see [docs/LINUX.md](docs/LINUX.md)).
- No external mod loader needed (`me3` is not required).

Steps:

1. If you have Sekiro Online or other online mods, remove them first. Delete `dinput8.dll`, `steam_api64.org`, `cream_api.ini` and `SekiroOnlineFont.ttf` from the game folder, then in Steam do Properties > Installed Files > Verify integrity.
2. Extract/drag `coop_sekiro_launcher.exe` and the `CoopSekiro` folder directly into your Sekiro game directory (next to `sekiro.exe`).
3. Open `CoopSekiro\sekiro_coop_settings.ini` and set the **same password** for everyone (`password = ...`). Co-op stays off while it's empty.
4. Everyone needs the exact same mod version.

## How to play

1. Make sure Steam is open, double-click `coop_sekiro_launcher.exe`, and load your save. (To play vanilla solo without mods, launch Sekiro normally from Steam.)
2. After the intro (you lose to Genichiro and wake up at Dilapidated Temple), rest at the temple idol. You get 3 items:
   - **Kindred Bell**: open your world (host)
   - **Kindred Charm**: find the host's world and join it
   - **Severed Thread**: leave co-op
3. Host uses the Bell, everyone else uses the Charm. The top left corner shows connection status.
4. To stop, everyone uses the Severed Thread (joiners first, host last), then Quit Game from the menu or Alt+F4. Quit Game is greyed out while you're still in co-op, that's on purpose. Don't close the game from Task Manager unless it's frozen, so `log.txt` can record clean vs crash exits.
   

### Connecting

- Default is Steam, and if that doesn't connect in 15 s it switches to direct UDP.
- Steam lobbies only work when Sekiro is owned on that Steam account and Steam is online. If Steam doesn't work for you, use direct UDP.
- The password only gets sent as a one-way hash.

#### Direct UDP, step by step

All of this is in `[ADVANCED]` in `CoopSekiro\sekiro_coop_settings.ini` (next to `sekiro_coop.dll`).

**Host:**

1. Set `transport = udp` (or leave `auto`, then it switches to UDP after 15 s).
2. Load your save and use the **Kindred Bell**. The address only shows up after the Bell, not at game start.
3. The mod tries to open port **28100 UDP** on your router (UPnP). If that works, the top left says `UDP host, join code 1.2.3.4:28100` and the same code is saved to `CoopSekiro\udp_join_code.txt` (deleted when the session closes).
4. No `udp_join_code.txt` and the top left says `UPnP failed`? Your router has UPnP off. Forward **UDP 28100** to your PC by hand in the router settings, and get your public IP from a site like whatismyip.com. Your join code is then `<that ip>:28100`.
5. Send the join code to everyone joining.

**Joiners:**

1. Set `transport = udp` and `udp_host_address = <host's join code>`, for example `udp_host_address = 1.2.3.4:28100` (just the IP works too, port 28100 is the default).
2. Same `password` as the host.
3. Load your save and use the **Kindred Charm**.

Only the host needs an open port. All joiners use the same address. If you change `udp_port`, everyone has to use the same port.

**Can't open ports?** Use a virtual LAN like Radmin VPN, ZeroTier or Tailscale. Everybody joins the same network, and joiners put the host's VPN IP in `udp_host_address`. No port forwarding needed then.

### Settings

Either edit `CoopSekiro\sekiro_coop_settings.ini` (it gets re-read every couple seconds) or use Options > **CooP Sekiro Settings** in game. The host's game rules (scaling, debuffs, sharing) apply to everyone. Every setting has a comment in the ini.

### Saves

CooP Sekiro uses `S0000.coop.sl2`, automatically copied from your normal `S0000.sl2` on first launch if missing. Your normal `S0000.sl2` isn't touched. Backups go to `%LOCALAPPDATA%\sekiro-coop\save-backups\` (10 newest kept). Still, keep your own backup before playing.

## Linux / Steam Deck

Not tested directly on Linux yet. You can run `coop_sekiro_launcher.exe` inside Sekiro's Proton prefix (via Steam Non-Steam Game + `STEAM_COMPAT_DATA_PATH` pointing to Sekiro's `compatdata/814380`, or via `protontricks-launch --appid 814380`). Full instructions are in [docs/LINUX.md](docs/LINUX.md).

## When something breaks

- Launcher error saying `sekiro.exe` or `CoopSekiro\sekiro_coop.dll` was not found: make sure both `coop_sekiro_launcher.exe` and the entire `CoopSekiro\` folder were extracted into your Sekiro game directory next to `sekiro.exe`, and check that antivirus did not quarantine `sekiro_coop.dll`.
- The log is at `%LOCALAPPDATA%\sekiro-coop\log.txt`.
- Press **F11** (or Back/Share on a controller) right when a bug happens to mark the log and dump state.

## For devs

All source and upstream reverse-engineering notes are included. Most original notes in `docs/` are in Finnish from Tihvert's upstream repo.

| Where | What |
|---|---|
| `crates/coop-launcher` | standalone Windows launcher (`coop_sekiro_launcher.exe`) that spawns `sekiro.exe` suspended and injects `CoopSekiro\sekiro_coop.dll` |
| `crates/coop-dll` | the mod DLL (save redirection, hooks, sync, boss rules, UI, settings) |
| `crates/coop-net` | network layer (UDP, password join, reliable messages, UPnP), unit tested |
| `crates/coop-audit` | log analyzer: compares the host's and friends' logs and finds desyncs |
| `vendor/sekiro` | vendored `sekiro` crate from fromsoftware-rs with a small fix (own license, see below) |
| `docs/` | research, reverse engineering notes, bug hunts, specs, test notes |
| `docs/JATKA-TASTA-MULTIPLAYER.md` | upstream dev log with version-by-version bug and log analysis |
| `docs/IMPLEMENTATION-NOTES.md` | how each system works and why |
| `docs/RE-*.md` | reverse engineering findings (offsets, structs, game functions) |
| `re/` | IDA and memory scan scripts + summary lists |
| `tools/icons/` | how custom item icons get made and injected |
| `ship/` | upstream internal tester scripts |
| `serve.ps1`, `test-server.cmd` | upstream tester log server for `coop-audit` |

Building: Windows, Rust stable (`rust-toolchain.toml`), Visual Studio 2022 (Build Tools or Community) with C++ and Windows 10/11 SDK.

```bat
build.cmd        rem release build of sekiro_coop.dll and coop_sekiro_launcher.exe
build-dist.cmd   rem release build into target-dist\
build-public.cmd rem clean release zip in dist\public\coop-sekiro-v<VER>.zip
test.cmd         rem unit tests (coop-net, coop-audit, coop-launcher, sekiro_coop --lib)
```

**Not included on purpose:** decompiled game code from IDA (FromSoftware's code) and archive keys needed by `tools/icons/build_icons.py`.

## Credits

- **Tihvert** — original creator of *Sekiro Together*, open-sourced under the MIT license, making **CooP Sekiro** possible.
- **naamanorsunvitulla** — upstream co-op tester and log contributor for Tihvert.
- [fromsoftware-rs](https://github.com/vswarte/eldenring-rs) by vswarte (`vendor/sekiro`).
- [hudhook](https://github.com/veeenu/hudhook) by veeenu (in-game overlay).
- LukeYui's Elden Ring Seamless Co-op and Dark Souls seamless co-op mods (drag-and-drop launcher & seamless co-op inspiration).
- Fork by **xCløuD**.

## License

MIT License, see [LICENSE](LICENSE). `vendor/sekiro` keeps its own license (MIT NON-AI OR Apache NON-AI, files in `ship-public/licenses/` / `vendor/sekiro`).

Unofficial fan mod. Not affiliated with FromSoftware, Activision or Valve, and not affiliated with LukeYui. Doesn't touch official servers. Use at your own risk and back up your saves.
