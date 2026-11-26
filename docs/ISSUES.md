# Known issues (driver, 2026-11-25)

Living list of problems in the replacement driver (`driver/`) found while
running it read-only and in controlled write windows. Each has the symptom,
the evidence, and the current status.

## 1. Battery e-ink display turns off when only our renderer drives it
- **Symptom:** when the driver owns `/dev/fb0` (vendor app not redrawing it),
  the physical e-ink panel on the battery goes blank/off.
- **Likely cause:** on the live unit `/dev/fb0` is a real **char device**
  (e-ink controller). The vendor app draws the 15001-byte 1bpp framebuffer
  *and* performs the panel refresh via its own path; our `display.rs` renderer
  writes pixels but the panel is not being refreshed/kept awake, so it sleeps.
  (On the emulator `/dev/fb0` is a regular file, which is why this never shows
  up on the bench.)
- **RE result (2026-11-25, app.bin):** there is **no custom e-ink ioctl and
  no MCU/SPI poke**. The vendor update path is
  `internal/display/abi`:
  `renderer.flushFrame` → `writeScreenData` (memmove packed pixels into the
  mmap) → `renderer.sync`; `renderer.sync` calls interface slot `+0x14`,
  which is the framebuffer `Sync` method = `mappedFramebuffer.Sync` =
  `os.(*File).Sync`. `openFramebuffer` is just `os.OpenFile("/dev/fb0")` +
  `syscall.Mmap(MAP_SHARED)`; `Close` only munmaps/closes. `libc`/unix
  ioctl appears in the binary only for generic `unix.ioctl*`; `app.bin` has
  no `FBIO*` constant. So the refresh is the **fsync of the fb fd / msync of
  the mapping**, which on the char device reaches the kernel fb driver's
  `fb_ops` fsync hook (which schedules the deferred screen copy). Our old
  `FBIOPUT_VSCREENINFO` "flush" was a no-op guess.
- **Fix:** `driver/src/display.rs` now `msync(MS_SYNC)`s the dirty mapping
  after each diff write when FBIOGET_* succeed (char device); on the
  emulator regular file it is a no-op (mode logged as `refresh=file`).
- **Status:** FIXED IN CODE, NEEDS LIVE CHECK. If the panel still stays
  dark, the remaining unknown is the kernel fb driver (not in app.bin);
  confirm with the read-only experiment below.
- **Live experiment (if still dark):** with the vendor app running
  (`app.backup &`), `strace -f -e trace=ioctl,sync,fsync,fdatasync,msync,openat -p $(pidof app)` and
  let one screen update happen — expect only `openat(/dev/fb0)`, `mmap`,
  `write`s and `fsync(fb0)`. If a custom ioctl does appear there, its number
  is the missing refresh; a one-shot poke is
  `python3 -c 'import fcntl,os; fd=os.open("/dev/fb0",os.O_RDWR); fcntl.ioctl(fd, 0x46XX)'`.
  Also `cat /proc/$(pidof emaldo-driver)/maps | grep fb0` + `ls -l /dev/fb0`
  confirms we hold it. Check the kernel module (`modinfo -F filename` of the
  fb driver, or `/sys/class/graphics/fb0/name`) for an fb_ops fsync/deferred_io.
- **Not a data problem:** telemetry/web/MQTT are unaffected; it is display-only.

## 2. Live inverter setpoint writes time out (control not executed)
- **Symptom:** in service mode (`writes_enabled=true`, `mode=optimize`) the
  controller reaches the write step but logs
  `all inverters failed: inv0/1/2 write timeout` and writes nothing.
- **Context:** a standalone `--write-probe` FC16 block write **succeeded** on
  USB3 earlier (0x0FDD 300 → read-back 300 → restore 0), and the block
  *snapshot* read now succeeds. So reads and one-off writes work; the
  continuous service-path write times out.
- **Suspects:** contention between the 3-port poll loop and the write
  (mutex is shared but timing/inter-round-trip), the write-all fan-out, or the
  inverter rejecting a repeated no-op FC16 with the same value.
- **Status:** ROOT CAUSE FOUND + FIXED on the emulator bench (not yet
  re-verified live). The timeout message was misleading: the writes never
  reached the bus.
  - **Cause:** in service mode `hardware.mcu_protocol` and
    `hardware.bus_enabled` are both true. The poll thread took
    `run_mcu()` first, which loops forever on the MCU Bmtp UART and
    therefore never reached the "open inverter ports / attach to `Bus`"
    code. The `Bus` stayed empty, `ensure_snapshot()` failed with
    "bus not connected yet", and once the snapshot existed every FC16 was
    rejected with `bus not connected` (logged as a per-inverter write
    failure). The write fan-out, FC16 framing and the shared mutex were
    never involved.
  - **Fix:** `poll.rs` only gives the poll thread to the MCU when the bus
    is disabled (`mcu_protocol && !bus_enabled`, live next to the vendor
    app). In service mode the MCU is polled once per bus tick with a
    persistent client (`poll_mcu_tick`), so battery SOC still arrives and
    the bus is attached/open for the controller. `mcu_proto` gained
    path/baud accessors and `read_snapshot_strict`, which reports a dead
    port (write/read error) as `Err` so the client is dropped and reopened
    instead of quietly timing out forever. The startup event claiming
    "mcu_protocol overrides bus_enabled" is corrected.
  - **Rollback:** `rollback()` now restores the setpoint block with one
    FC16 per port (FC06 on the block failed live) and verifies every
    register on every attached inverter instead of only the primary port
    (`bus::read_block_all/write_block_at/write_single_at/read_holding_at`).
  - **Bench evidence (3 fakes @9600 + fake MCU, optimize mode):**
    `writes.log` shows `0x0fdd 300 -> written+verified`, each fake logs
    `WRITE multi 0x0fd8 x6 = [3600, 3600, 0, 0, 0, 300]`, and rollback
    restores `[... 3600]` on all three with `written+verified` per
    register. `cargo test --offline`: 73 passed.
  - **Still to do live:** confirm the MCU and the three inverter TTYs are
    genuinely independent on the hardware (different CH9344 ports) before
    enabling both.

### 2b. Follow-up: the live FC16 write still timed out (2026-11-26, SOAK)

- **Symptom:** after the fix above the ports open and the snapshot works,
  but on the live unit every service-mode write still failed:
  `{"register":"0x0fdd","value":0,"result":"error: all inverters failed:
  [\"inv0: write timeout\", \"inv1: write timeout\", \"inv2: write
  timeout\"]"}`. A standalone `--write-probe` FC16 on one port works.
- **Root cause (bench-reproduced, ranked):**
  1. **A one-register FC03 is structurally undecodable.** A 1-register
     read request is 7 bytes; the inverter expects 8-byte requests, so it
     swallows the first CRC byte and answers with shifted framing. On the
     bench the read-back of the write returned another register's value
     (`SOC 900` for `0x0FDD`) - exactly the "stale" symptom. The write
     fan-out verified with that single-register read and also asked for
     `0x0FDD` *immediately after* an FC16, so any late/echo byte mixed in
     made it time out. **This is the main cause.**
  2. **Fixed-size reads on a slow slave.** The old reader expected exactly
     `8` bytes and poked byte 1 after the first chunk; a reply that
     arrives late, split, or with a torn tail in front was mistaken for a
     stale answer (`BadLength`/`timeout`). Timeouts also counted the TX
     time, and there was no TX drain or inter-frame gap.
- **Fix:**
  - `modbus.rs`: self-framing reader (CRC-checked frame anywhere in the
    buffer, leading garbage skipped, torn frames completed inside the
    budget, oversized buffers reset), TX drained before the response
    budget starts, inter-frame gap, explicit `Stale` error, and
    `reject_single()`. A one-register FC03 request is now refused for
    **every** caller (the probe reads an aligned 4/6-register block).
  - `bus.rs`: the setpoint fan-out verifies the read-modify-write with
    the **validated 8-byte FC16 echo** (address + count) instead of a
    second, single-register read; per-port block readers and a
    drain-and-retry on `Stale`; the poll read of one port is contiguous.
  - `control.rs`: rollback restores the setpoint block with one FC16 per
    port and confirms the out-of-block registers with their **FC06 echo**
    (which echoes address and value); nothing reads 0x0FD3..0x0FD7
    (write-only) any more.
  - `config.rs`: `[control] inverter_timeout_ms` (default 1500, measured
    from after TX) and `skip_noop_writes` (default true).
- **Bench (3 slow reference fakes, +300-1500 ms per reply, optimize mode,
  override slots = 10 W to guarantee a non-zero setpoint):**
  `0x0fdd 300 -> written+verified` on all three fakes, and rollback
  `{"ok":true,"result":"restored 7 registers"}` with every register
  `written+verified`. `cargo test --offline`: 85 passed.
- **Live step for the lead (one port, one-shot, no service mode):**
  1. Read-only sanity: `emd2-drv --config <cfg> --state-dump` (uses the
     block read) run twice; the setpoint block must be identical and no
     `read ... FAILED`/`stale` events should appear in the events list.
  2. One-shot FC16 on **one** port with a value that really changes:
     `WRITE_PORT=<port> WRITE_FC=16 WRITE_REG=0x0FDD WRITE_HOLD=300`
     (driver not running on that port; the probe reads back, then writes
     back 0 / the original). Expect `{"step":"read_after",... "300"}` and
     no timeout. If it still times out while `--write-probe` with a
     single port succeeded before, capture `strace -e trace=read,write,ioctl
     -f` of that probe to see the exact request/response bytes.

## 3. PV power reads 0 / not parsed from inverter (daytime load understated)
- The provisional inverter register map is off by default (`map_unvalidated`),
  so live PV power comes from the inverter block only when enabled. Grid comes
  from the b-sensor, battery from CAN, so overnight the derived load
  (`PV + grid − battery_charge`) is accurate; **during PV production the
  derived load understates** until PV is parsed from the inverter blocks
  (0x0FA0×43). See `REGISTERS.md`.
- The Open-Meteo forecast (`driver/src/solar.rs`) is planner input only, not a
  live power reading, so it does not close this gap.
- **Status:** OPEN (mapping validation pending).

## Notes
- Load is derived when no direct reading exists: `load = PV + grid(net) -
  battery(+charge)` (`telemetry::derive_load`), recomputed on every CAN/grid
  update so it never uses a stale battery value.