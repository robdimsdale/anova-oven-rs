# The oven's ESP32 console, over the network

Every so often the oven drops off WiFi and app pairing can't bring it back. The
only recovery so far has been to open the oven, clip a debug probe onto the
ESP32's serial header and type a command to set the WiFi credentials again.

This document records that header, how a Raspberry Pi is wired to it so the
oven never needs opening again, and the bridge in `uart-bridge/` that keeps the
console reachable over the network and logs everything it prints. The logging is the
point as much as the remote access: until now we have only ever seen the oven
*after* it dropped off, never the minutes leading up to it.

## The header

The ESP32 is an **ESP32-WROOM-32U** (original ESP32, external antenna). It is
not just a WiFi module: its console drives the heating elements, fan, valves
and boiler directly, and talks to a separate Renesas UI board, so it is
effectively the oven's main controller (see "The console" below). Next to it is a 5-pin header marked `ESP32`, labelled in this order:

| Label | Signal | Notes |
|---|---|---|
| GND | Ground | |
| RXD0 | ESP32 UART0 RX (GPIO3) | Input. Labels are from the ESP32's side. |
| TXD0 | ESP32 UART0 TX (GPIO1) | Output, idles high once booted. |
| IO2 | GPIO2 | Strapping pin. Not connected. |
| EN | Chip enable / reset | Pulled up; low holds the ESP32 in reset. Not connected. |

Logic is 3.3V. Check it on **EN**, not TX: EN sits at the ESP32's supply rail
and doesn't switch, whereas a DC meter on a busy TX line reads the average.

**The console runs at 230400 baud, 8N1.** The ROM boot banner
(`rst:0x1 (POWERON_RESET),boot:0x13 …`) still comes out at 115200, so a power
cycle shows a few lines of garbage before the application's output, at 230400,
starts.

## Wiring the Pi

A Raspberry Pi 2B. It has no Bluetooth, so the full PL011 UART is on
GPIO14/15 already and no `disable-bt` overlay is needed. It has no WiFi
either, which suits this job: it's on ethernet, so it doesn't share whatever
takes the oven off the wireless network.

| Pi physical pin | Pi signal | Oven header |
|---|---|---|
| 6 | GND | GND |
| 8 | GPIO14 TXD | RXD0, through 1kΩ |
| 10 | GPIO15 RXD | TXD0, through 1kΩ |

The series resistors limit back-powering when one side is on and the other is
off. They make no difference at 230400.

Pi side, once: `raspi-config` → Interface Options → Serial Port, **No** to the
login shell and **Yes** to the hardware. That sets `enable_uart=1` and takes
`console=serial0` out of `cmdline.txt`, so the Pi doesn't print its own boot
log into the ESP32's RX. Check with `ls -l /dev/serial0` (→ `ttyAMA0`) and
`pinctrl get 14-15` (both `a0`, `TXD0`/`RXD0`).

### When the console is silent

Silence (rather than garbage) means nothing is arriving on GPIO15, not a wrong
baud rate. `pinctrl` can see the line directly. GPIO15 has no pull by default,
so a floating pin reads anything; add a pull-down first:

```sh
pinctrl set 15 a0 pd
pinctrl get 15      # hi: the oven's TX is driving the pin. lo: it isn't.
```

The Pi 2's SoC can't read pull settings back, so `pinctrl` always shows `--`
for the pull even after this. Undo it with `pinctrl set 15 a0 pn`, or reboot.

Garbage means the link works and the baud rate is wrong.

### Power

The first Pi supply was marginal. Under load the SD card dropped off the bus
mid-session: every command not already cached came back
`command not found`, and the next boot showed nothing on HDMI. The card itself
was fine (clean `dd` read; `e2fsck` found only the marks of an unclean
shutdown). The official supply fixed it, leaving one brief dip during boot.

```sh
vcgencmd get_throttled   # 0x0 is clean. Bit 0: under-voltage now; bit 16: since boot.
```

Fitted inside the oven, the Pi should take 5.1V from a buck converter into
GPIO pins 2/4 rather than through its micro-USB socket, and
`get_throttled` is worth watching.

## The bridge

```
/dev/serial0 ──ser2net── :2323 ──┬── oven-uart-log.service ──▶ journal
                                ├── interactive session (nc)
                                ├── monitoring
                                └── spare
```

Only one process can hold a serial port, so `ser2net` owns it and serves it
as a raw TCP port that allows four connections at once: one permanently held
by the logger, one for an interactive session, one for monitoring and one
spare. Output from the oven goes to all of them, and input from any of them
reaches the oven.

The port listens on every interface and has no authentication: anyone on the
LAN can connect and run any console command, which includes driving the
heating elements and boiler directly, factory-resetting the oven and pointing
it at new firmware. That's accepted for a home network. To lock it down, bind it to loopback (`tcp,127.0.0.1,2323`) and
reach it through `ssh -L 2323:127.0.0.1:2323 anova-direct` instead.

The logger is `socat` piped into the journal, which timestamps every line,
survives reboots (`journald-oven.conf`) and caps itself at 200MB. Using the
journal instead of files means no logrotate and `journalctl --since` for
free.

### Installing

On the Pi, from a checkout of this repo:

```sh
sudo uart-bridge/install.sh
```

### Using it

Follow the console live, or look back at a dropout:

```sh
ssh anova-direct journalctl -u oven-uart-log -f
ssh anova-direct journalctl -u oven-uart-log --since "2026-10-14 03:00" --until "2026-10-14 03:30"
```

Talk to it interactively:

```sh
nc anova-direct 2323
```

`nc` sends a line at a time on Enter. For a raw terminal, where control keys
reach the oven, use `socat -,raw,echo=0,escape=0x1d TCP:anova-direct:2323`
(`ctrl-]` disconnects). Commands typed here are also visible in the log
(the console echoes them).

To use `tio` directly on the serial port instead, stop the bridge first,
since it holds the port: `sudo systemctl stop oven-uart-log ser2net`, then
`tio -b 230400 /dev/serial0`.

## The console

The console echoes input, so commands appear in the log alongside their
output. Arguments are space-separated.

### What healthy looks like

Idle and connected, the oven prints a cloud healthcheck every 10 seconds:

```
W (1139499) AWS: Received(op=1,fin=1)={"id":"14cceb10-…","type":"CMD_APO_HEALTHCHECK"}
W (1139501) AWS: Total payload length=74, data_len=74, current payload offset=0

W (1139508) AWS: iot loop msg=9, count=195, free heap=86556
OD LOCKED!
command: 'CMD_APO_HEALTHCHECK', payload @ 0x0, active_state @ 0x0
OD UN-LOCKED!
W (1139512) AWS: iot loop msg=5, count=196, free heap=86560
```

The number in brackets is milliseconds since boot. `CMD_APO_HEALTHCHECK`
arrives over the same AWS IoT channel as the commands in
[oven-websocket-api.md](oven-websocket-api.md), so it's a direct signal of
cloud connectivity: **the healthchecks stopping is the moment to look at** when
the oven drops off. The `free heap` figure sits around 85–88KB here; a steady
decline across a day would point at a leak rather than the network.

### Recovering WiFi

```
WiFi                          # current WiFi info
WiFi connect <ssid> <passwd>
```

This is the command the debug probe was used for. Since arguments are split on
spaces, an SSID or password containing a space probably can't be entered.

### Commands for diagnosis

Safe to run; they only read state or change logging.

| Command | Use |
|---|---|
| `build` | Firmware build and partition info. |
| `dumpErrorStates` | Current state of every error flag. |
| `HeapAvailable` | Free heap; worth tracking if dropouts follow a slow leak. |
| `iot getconfig` | Cloud/IoT configuration. |
| `logLevel <TAG> <level>` | ESP-IDF log level per tag (`verbose` … `none`). Raising the WiFi tags is the obvious next step for catching a dropout. |
| `DataLogs <on/off>` | Periodic data logging. |
| `door read`, `t`, `watts`, `stickyTemps`, `tank` (no args) | Sensor readings. |

### Commands to leave alone

These drive hardware directly, bypassing whatever the normal cook logic
enforces, or change persistent state: `Triac`, `DCout`, `Fan`, `Valve`,
`setBoiler`, `boilerDoseTime`, `master`, `oven`, `setTopTemp`, `setBotTemp`,
`gpio set`, `dac`, `factoryReset`, `FirmwareUpgrade`, `uiUpd`, `uiOTA`,
`files delete`, `userId set|store`, `tank <n>`, `descaleNeededForce`,
`mfgOutputBlock`, `manuStageStart`, `unittest`.

### Full `help` output

```
build                   - print build and partition info
dac                     - set dac out (audio) value (0-255)
DCout                   <dc_num> <set_level>
door                    read
Fan                     <percent>
led                     [on|off] <ledName>
logLevel                <LOG_TAG> (verbose|debug|info|warn|error|none)
master                  [on|off]
oven                    [start|stop|select|temp] [...]
t                       get probe temperature(s)
Triac                   <index> <percent>
WiFi                    get current wifi info           connect <ssid> <passwd>
Valve                   <valve_num> <time-ms-open>      0->close now, -1->leave open
watts                                   [current power consumption]
HeapAvailable           - Show the available amount of heap
FirmwareUpgrade         -<url> OR nightly
DataLogs                - <on/off>       turn data logs on off
manuStageStart          - <test name | help> <args> Starts a manufacturing test
dumpErrorStates         - Print the current state of all errors
spkr_off                - <0|1>          Toggle the SPRK_OFF
mfgOutputBlock          - <0|1>          Turns on (1) or off (0) the output blocker which will only allow manufacturing test ouput through.
factoryReset            - Initiates a factory reset.
descaleBoilerTracker     Shows how long the boiler has been running for.
setBoiler                <boiler power percent>          turns the boiler power to the indicated percentage
boilerDoseTime           <boiler dose msec>      set how long the boiler dose valve is open for a dose (msec)
tank                    - [<0-5>]        No args will return current level, 0-5 will set the current level.
lampPref                - <on|off>       sets the lamp preference
random                  - use ATECC device to generate a 128bit random number
stickyTemps             - Prints current sticky temps
userId                  - < set emailAddr | get | store >
spiffsCurrentFiles      - Displays current files held in spiffs
files                   - <dump|delete|size>
gpio                    - <set|get> [analog] <address> <set:level>
iot                     - <getconfig|help>
keycode                 - <code>         shows 4 digit code on display
uiOTA                   - sends dummy data to UI to test flash
uic                     - commands to renesas UI board
uiUpd                   - <start/reset/chunk> <firmware size/(nothing needed)/chunk data>
unittest                - run unit testing
descaleNeededForce      - Sets the boiler time to 30hrs, turns on the descale needed led.
setTopTemp              - temp
setBotTemp              - temp
```

## Still to do

- **Find out why the oven drops off.** Next time it happens, line up the
  journal around the dropout with the router's logs.
- **Reset over EN.** Wiring EN to the Pi through an open-drain stage (a
  transistor that can only pull low) would allow resetting the ESP32 remotely
  without the Pi's boot-time pin states ever affecting it. Since the ESP32 is
  the main controller, a reset presumably abandons any cook in progress.
