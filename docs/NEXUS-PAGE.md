# Nexus Mods page (copy-paste)

Category: Gameplay. Upload `coop-sekiro-v1.2.0-beta.zip` as the main file (version `1.2.0-beta`), and the pics from `media/` (when taken) as images.

## Name

CooP Sekiro

## Summary (max 350 characters)

Online co-op mod for Sekiro: Shadows Die Twice, forked from Sekiro Together by Tihvert and maintained by xCløuD. Simple drag-and-drop installation with a standalone launcher (no me3 required). 2-6 players in the same world with separate vanilla and co-op saves. Requires Sekiro 1.06 on Steam.

## Description (BBCode)

```bbcode
[size=6][b]CooP Sekiro[/b][/size]
By [b]xCløuD[/b] — forked from [i]Sekiro Together[/i] by [b]Tihvert[/b].

[b]Read this before you install.[/b] The mod is a beta and still has rough edges, so back up your saves first. Only play with people you trust: Sekiro never got the netcode security patches the other Souls games received.

[size=5][b]About[/b][/size]

[size=4][b]Co-op & Drag-and-Drop Setup[/b][/size]
Drop [b]coop_sekiro_launcher.exe[/b] and the [b]CoopSekiro[/b] folder directly into your Sekiro directory next to sekiro.exe — no external mod loader (me3) required. Launching Sekiro normally from Steam runs vanilla unmodded Sekiro; running [b]coop_sekiro_launcher.exe[/b] launches CooP Sekiro with its own separate save file ([b]S0000.coop.sl2[/b]).

[list]
[*] 2-6 players: one host, everyone else joins the host's world.
[*] Enemies and bosses synced, plus bosses, idols, shortcuts and NPC quest progress.
[*] Resting respawns enemies for everyone.
[*] Shared XP, sen, drops, boss rewards and key items (configurable).
[*] Co-op uses its own save file ([b]S0000.co2[/b]), your vanilla save isn't touched. Auto backups every 10 min.
[/list]

[size=4][b]Dying and bosses[/b][/size]
[list]
[*] If you die in a boss fight you wait or spectate the others until the boss is dead or everyone is. The spectate camera can switch between alive players.
[*] Resurrection works like normal. If everyone dies it's a wipe.
[*] A boss keeps lost health and lives when only one player dies.
[*] Deathblow lock, so two people can't deathblow the same enemy at once.
[/list]

[size=4][b]Balancing & Quality of Life[/b][/size]
[list]
[*] Adjustable enemy/boss scaling and optional death debuff (host's values apply to everyone).
[*] Party HP bars for the other players.
[*] CooP Sekiro Settings page inside the game's own Options menu.
[*] Optional 120 fps unlock & custom icons for the co-op items.
[*] Ultrawide support planned for the future.
[/list]

[size=4][b]Known issues (inherited from upstream, have attempted to get these fixed but needs further testing before I can cross these off)[/b][/size]
[list]
[*] Enemies or bosses can desync now and then.
[*] Levers, lifts and breakables are not synced; doors only partially.
[*] Resurrect / spectate camera edge cases are still being hardened.
[*] Story phases, Dragonrot, Bell Demon and cutscenes aren't handled specially yet.
[*] Host stutter spikes can occur during area transitions / heavy combat; 3-6 players not tested live yet.
[/list]


[size=5][b]Can this ban me?[/b][/size]
Sekiro doesn't have an online multiplayer server or anti-cheat. CooP Sekiro uses its own connection and its own save file (S0000.coop.sl2).


[size=5][b]Installation[/b][/size]
You need:
[list]
[*] Sekiro on Steam, version [b]1.06[/b] (normal unmodded exe). Only the Steam version is supported.
[*] Windows (or Linux / Steam Deck with Proton). No me3 required.
[/list]

[list=1]
[*] Got Sekiro Online or other online mods? Delete dinput8.dll, steam_api64.org, cream_api.ini and SekiroOnlineFont.ttf from the game folder, then Verify integrity of game files in Steam.
[*] Extract [b]coop_sekiro_launcher.exe[/b] and the [b]CoopSekiro[/b] folder directly into your Sekiro game directory (next to sekiro.exe).
[*] Open [b]CoopSekiro\sekiro_coop_settings.ini[/b] and put the same password for everyone (password = ...). Co-op stays off while it's empty.
[*] Everyone needs the exact same mod version.
[/list]


[size=5][b]Use[/b][/size]
[list=1]
[*] Start Steam, double-click [b]coop_sekiro_launcher.exe[/b], and load your save. (To play normal solo Sekiro, launch through Steam as usual.)
[*] After the intro (Genichiro beats you and you wake up at Dilapidated Temple), rest at the temple idol. You get 3 items:
[b]Kindred Bell[/b] - open your world (host)
[b]Kindred Charm[/b] - find the host's world and join it
[b]Severed Thread[/b] - leave co-op
[*] Host uses the Bell, everyone else uses the Charm. Connection status is in the top left corner.
[*] To stop, everyone uses the Severed Thread (joiners first, host last), then Quit Game or Alt+F4. Quit Game is greyed out while you're still in co-op, that's on purpose.
[/list]

Settings: edit [b]CoopSekiro\sekiro_coop_settings.ini[/b] or use Options > CooP Sekiro Settings in game.
Connection is Steam by default and falls back to direct UDP after 15 s. Steam lobbies work when Sekiro is owned on that Steam account and Steam is online; otherwise use direct UDP:


[size=5][b]Direct UDP[/b][/size]
Everything here is in [ADVANCED] in CoopSekiro\sekiro_coop_settings.ini.

[b]Host:[/b]
[list=1]
[*] Set [b]transport = udp[/b] (or leave auto, then it switches to UDP after 15 s).
[*] Load your save and use the [b]Kindred Bell[/b]. The address only shows up after the Bell, not at game start.
[*] The mod tries to open port [b]28100 UDP[/b] on your router (UPnP). If that works, the top left says "join code 1.2.3.4:28100" and saves it to [b]CoopSekiro\udp_join_code.txt[/b] (deleted when closed).
[*] No udp_join_code.txt and "UPnP failed"? Forward [b]UDP 28100[/b] manually and check your public IP.
[*] Send `<ip>:28100` to everyone joining.
[/list]

[b]Joiners:[/b]
[list=1]
[*] Set [b]transport = udp[/b] and [b]udp_host_address = <host's join code>[/b], e.g. 1.2.3.4:28100.
[*] Same password as the host.
[*] Load your save and use the [b]Kindred Charm[/b].
[/list]

[b]Can't open ports?[/b] Use Radmin VPN, ZeroTier or Tailscale and put the host's VPN IP in udp_host_address.


[size=5][b]Credits[/b][/size]
[list]
[*] [b]Tihvert[/b] — original creator of [i]Sekiro Together[/i] (open source MIT base)
[*] [b]xCløuD[/b] — maintainer of the [i]CooP Sekiro[/i] fork
[*] [b]naamanorsunvitulla[/b] — upstream co-op testing and logs
[*] vswarte for fromsoftware-rs (sekiro crate)
[*] veeenu for hudhook
[*] LukeYui for the Seamless Co-op launcher & co-op design inspiration
[/list]

Unofficial fan mod, not affiliated with FromSoftware, Activision or Valve. Use at your own risk.
```
