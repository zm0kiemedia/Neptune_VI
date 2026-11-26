<p align="center">
  <img src="docs/assets/banner.svg" alt="Neptune VI" width="100%">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Windows%2011-0b0a15?style=flat-square&logo=windows11&logoColor=white" alt="Windows 11">
  <img src="https://img.shields.io/badge/Ryzen%207%209800X3D-0b0a15?style=flat-square&logo=amd&logoColor=ed1c24" alt="Ryzen 7 9800X3D">
  <img src="https://img.shields.io/badge/RTX%205070%20Ti-0b0a15?style=flat-square&logo=nvidia&logoColor=76b900" alt="RTX 5070 Ti">
  <img src="https://img.shields.io/badge/GPL--3.0-0b0a15?style=flat-square&logo=gnu&logoColor=white" alt="GPL-3.0">
</p>

<p align="center">
  Runs the Xbox Series build of <b>GTA VI</b> on a Windows PC.<br>
  A port of <b>Neptune (VIctory Edit.)</b> by <b>Traffic</b> &amp; <b>imgone</b> for AMD Ryzen + NVIDIA.
</p>

<br>

## ◆ Why this port

Upstream froze the GPU right before the first gameplay scene on a Ryzen 7 9800X3D with an RTX 5070 Ti. The cause: the game's indirect draw arguments ran unchecked, and some of them hold garbage.

This port sanitizes those records once, right after the GPU work that writes them, and then lets the draws run directly. Broken records become empty draws instead of hanging the GPU. **The game gets into gameplay, at the same speed as the unchecked mode** (4.6 fps with per-draw checks, 8.4 fps with this, measured at one save).

<br>

## ◆ Ryzen notes

| | |
|:--|:--|
| **Console instructions** | The Xbox CPU is a Zen 2. Instructions Intel lacks (`MONITORX`, `MWAITX`, `CLZERO`, `INSERTQ`) have to be emulated on Intel. A Ryzen has them in hardware. |
| **Indirect draws** | `VICTORY_CLAMP_AFTER_WRITER=1` + `VICTORY_INDIRECT_RAW=3` are the default: argument records are sanitized once after the GPU writes them, then run raw. `-RawIndirect` brings back upstream's unchecked mode. |
| **Black sky frames** | Fixed: a replaced texture keeps showing until its new version is uploaded. |
| **Water physics** | On by default: the ocean's height maps are copied back into game memory, so boats float and you can swim. `-NoOcean` turns it off. |
| **32 GB RAM** | Upstream was tuned on 64 GB. `-Ram32` shrinks the list pool, the texture staging and the card copies. |

<br>

## ◆ How to start

> [!IMPORTANT]
> No game files are included. You need your own copy of the game.

**1 · Get the files into one folder**

```
D:\Neptune\
├─ victory.exe      ← build it (see Build below), or take it from Releases if one is published
└─ play-zm.ps1      ← from tools\ in this repo
```

**2 · Open PowerShell in that folder**

In Explorer, open the folder, type `powershell` into the address bar and press Enter.
If scripts are blocked, run this once for the window:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
```

**3 · First start: translate the shaders**

```powershell
.\play-zm.ps1 -Game 'D:\GTA6\game_scarlett_master.exe' -SeriesX -Warm
```

`-Warm` translates every shader of the game into the cache first (about 7 minutes, progress shows in PowerShell), then starts the game. You only need it once, and again after updating Neptune.

**4 · Every start after that**

```powershell
.\play-zm.ps1 -Game 'D:\GTA6\game_scarlett_master.exe' -SeriesX
```

Replace `D:\GTA6\...` with the path to **your** `game_scarlett_master.exe`.

<details>
<summary><b>All switches</b></summary>
<br>

| Switch | Does |
|:--|:--|
| `-Game '<path>'` | your `game_scarlett_master.exe` |
| `-Warm` | translate all shaders first (first start, and after updates) |
| `-SeriesX` | 4K Series X path (recommended) |
| `-Ram32` | smaller buffers for PCs with **32 GB** RAM (leave it out on 64 GB) |
| `-Stats -Tag x` | writes `statsx.log` and `memwatchx.csv` next to the script, handy for bug reports |
| `-Dred` | if the GPU hangs, the log names the shader that caused it |
| `-GpuTime` | logs the 25 most expensive GPU passes every 5 s |
| `-NoAutoA` | do not tap A through the start screens automatically |
| `-NoOcean` | turn off the water physics fix (boats sink) |
| `-Env 'NAME=value;...'` | any extra `VICTORY_*` setting |

Note: it is `-Warm`, one dash. `victory.exe warm` (no dash) does the same without starting the game.

</details>

<details>
<summary><b>Something went wrong?</b></summary>
<br>

| Problem | Try |
|:--|:--|
| Script won't run | `Set-ExecutionPolicy -Scope Process Bypass`, then start again |
| "Not found" | the `-Game` path is wrong, put it in single quotes |
| Crash in `amdxc64.dll` | AMD driver issue, this port is tested on NVIDIA only. Update the driver, or use an AMD-focused fork |
| Black or missing objects after an update | start once with `-Warm` |
| Crash or freeze | run with `-Stats -Tag bug` and attach `statsbug.log` and `%LOCALAPPDATA%\victory\data\7952987A\crashes.log` to an issue |

</details>

**Windows 10** also works: run `cargo xtask agility` once (it fetches Microsoft's D3D12 Agility SDK, hash-checked) and keep the `D3D12` folder next to `victory.exe`. Windows 11 needs nothing.

**Controls** · F9 mouse look · WASD move · Enter / Backspace / X / Y as A / B / X / Y · F10 overlay · F12 screenshot

<br>

## ◆ Overlay &amp; screenshots

A small translucent performance panel by **zm0kie** sits in the top-left corner of the game window. It shows FPS and frame time, the high, low and average of the last 30 seconds, draws per frame, CPU and GPU load, RAM and VRAM, and your hardware.

| Key | |
|:--|:--|
| `F10` | show / hide the overlay |
| `F12` | save a screenshot to `Pictures\Neptune` (with the overlay in the picture while it is shown) |

`VICTORY_OVERLAY=0` turns it off completely.

<br>

## ◆ Under the hood

<details>
<summary><b>Where a frame goes</b> &nbsp;·&nbsp; open world, about 6,500 draws, 90 ms (upstream measurement)</summary>
<br>

| Part | Share |
|:--|--:|
| Render thread in the submit step (memory copies, heap sync, write watch, textures) | ~41 % |
| Render thread waiting for the game's recording threads | ~48 % |
| Game's own code on the render thread | ~11 % |
| GPU busy | 35 % |

The CPU side is the limit, not the GPU.

</details>

<details>
<summary><b>Switches worth knowing</b></summary>
<br>

| Switch | Default | Does |
|:--|:--|:--|
| `VICTORY_AMBIENT` | `0.6` | ambient light level in place of ray-traced light; `off` disables it |
| `VICTORY_PIPELINE_WAIT_MS` | `2500` | longest a frame waits for a shader still compiling |
| `VICTORY_LEVELS_ON_DEMAND` | `1` | textures take card memory only for the mip levels in use |
| `VICTORY_VRAM_IDLE_MS` | `15000,2500,1000` | how long an unused texture stays on the card |
| `VICTORY_HEAP_SYNC` | `1` | mirrors descriptor records the game copies as bytes |
| `VICTORY_INDIRECT_RAW` | `3` | `0` clamp all, `2` trust all, `3` check on the CPU |

Pass any of them with `-Env 'NAME=value'`. Changing the shader translator or its switches needs a new `-Warm`.

</details>

<br>

## ◆ Build

```powershell
cargo build --release -p victory
```

Needs Visual Studio Build Tools (MSVC + Windows SDK) and [rustup](https://rustup.rs). Put `target\release\victory.exe` next to `tools\play-zm.ps1`.

<br>

## ◆ Known limits

- Ray-traced light is not running yet. An ambient pass stands in, so some rooms and water look off.
- Frame rate dips while the world streams in.
- Tested on one PC: Ryzen 7 9800X3D · RTX 5070 Ti 16 GB · 32 GB RAM.

<br>

## ◆ Credits

**Traffic** &amp; **imgone** for Neptune, the base of this port.
Built on Marvisak/VIctory · fxceless &amp; myokex (base, levels on demand, heap sync; PRs by Jonirulah, Flyander, AscilCH) · soldsawn (shader translator) · Belobug &amp; ShakiraRED-dt (ambient pass) · ZSlayerHQ · JoshuuDev · skornuta · Trexies · the DexNom FidelityFX patch · domints (Windows 10 Agility SDK loader, fxceless/victory-fork#402). Full list in [`docs/NEPTUNE-README.txt`](docs/NEPTUNE-README.txt).
Port by **zm0kie**.

<br>

<p align="center">
  <sub>GPL-3.0-or-later · not affiliated with Rockstar Games, Take-Two Interactive or Microsoft</sub>
</p>
