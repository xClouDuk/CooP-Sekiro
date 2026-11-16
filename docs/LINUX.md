# CooP Sekiro on Linux / Steam Deck

By **xCløuD** — forked from *Sekiro Together* by **Tihvert**.

**Untested directly on Linux.** Since **CooP Sekiro** uses a standalone Windows launcher (`coop_sekiro_launcher.exe`) + DLL (`CoopSekiro\sekiro_coop.dll`) — similar to LukeYui's `ersc_launcher.exe` — you run `coop_sekiro_launcher.exe` inside Sekiro's Proton prefix (`814380`) instead of needing `me3`.

You do **not** need `WINEDLLOVERRIDES="dinput8.dll=n,b" %command%`.

## What you need

- Sekiro on **Steam, version 1.06** (normal unmodded exe).
- Steam running with Proton enabled for Sekiro.

## Install & Play

1. Start Sekiro once from Steam the normal way, reach the main menu, then quit. This creates Sekiro's Proton prefix (`814380`) and save folder.
2. Got Sekiro Online or other online mods? Delete `dinput8.dll`, `steam_api64.org`, `cream_api.ini` and `SekiroOnlineFont.ttf` from the game folder, remove any old `WINEDLLOVERRIDES` launch option, then Steam > Sekiro > Properties > Installed Files > Verify integrity.
3. Extract `coop_sekiro_launcher.exe` and the `CoopSekiro` folder directly into your Sekiro game directory (next to `sekiro.exe`), e.g. `~/.local/share/Steam/steamapps/common/Sekiro/`.
4. Open `CoopSekiro/sekiro_coop_settings.ini` and set the same `password = ...` for everyone. Co-op stays off while empty.
5. Launch `coop_sekiro_launcher.exe` inside Sekiro's Proton prefix:
   - **Recommended (Steam & Steam Deck Game Mode)**: In Steam, click **Games > Add a Non-Steam Game to My Library...**, browse to `coop_sekiro_launcher.exe` in your Sekiro folder, open **Properties > Compatibility** and enable Proton (matching Sekiro's Proton version), then set **Launch Options** to:
     ```bash
     STEAM_COMPAT_DATA_PATH="$HOME/.local/share/Steam/steamapps/compatdata/814380" %command%
     ```
     (Adjust the path if your Steam library is on an SD card, another drive, or Flatpak Steam.)
   - **Alternative CLI (`protontricks`)**:
     ```bash
     protontricks-launch --appid 814380 "/path/to/Sekiro/coop_sekiro_launcher.exe"
     ```
6. Launching Sekiro normally from Steam plays unmodded solo; launching your **CooP Sekiro** shortcut runs co-op.

## Steam Deck tips

- **View button** (left of screen / Back on Xbox) works like F11 to mark bugs in `log.txt`.
- If the Deck gets warm or battery drains fast at 120 fps, set `fps_unlock = 0` in `CoopSekiro/sekiro_coop_settings.ini`.

## Where files live on Linux

`sekiro_coop_settings.ini` and `udp_join_code.txt` stay in `CoopSekiro/` inside your Sekiro game folder. Everything else is inside Sekiro's Proton prefix:

```text
~/.local/share/Steam/steamapps/compatdata/814380/pfx/drive_c/users/steamuser/
  AppData/Local/sekiro-coop/                      log.txt, log.old.txt, save-backups/
  AppData/Roaming/Sekiro/<your SteamID64>/        S0000.sl2 (normal save), S0000.coop.sl2 (co-op save)
```

Back up `AppData/Roaming/Sekiro` before playing, same as on Windows.

## Connecting

- Steam lobbies work between Linux/Proton and Windows players.
- Direct UDP requires UDP port **28100** open on the host (`sudo ufw allow 28100/udp` or `sudo firewall-cmd --add-port=28100/udp`), or a virtual LAN such as ZeroTier / Tailscale.
