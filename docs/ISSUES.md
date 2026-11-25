# Known issues (driver takeover / soak, 2026-11-25)

Living list of problems found while running our driver as the master (vendor
app retired). Each has the symptom, the evidence, and the current status.

## 1. Battery e-ink display turns off when the vendor app is not running
- **Symptom:** after a takeover (vendor `app` → `app.backup`), the physical
  e-ink panel on the battery goes blank/off.
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
- **Status:** OPEN. Debug on the bench against `fake_inverter_stateful.py`
  (records every frame) before another write-verifying live soak.

## 3. PV power reads 0 / not parsed from inverter (daytime load understated)
- The provisional inverter register map is off by default (`map_unvalidated`),
  so PV, and any direct load, come from the inverter block only when enabled.
  Grid comes from the b-sensor, battery from CAN, so overnight the derived
  load (`PV + grid − battery_charge`) is accurate; **during PV production the
  derived load understates** until PV is parsed from the inverter blocks
  (0x0FA0×43). See `REGISTERS.md`.
- **Status:** OPEN (mapping validation pending).

## Notes
- Load is derived when no direct reading exists: `load = PV + grid(net) -
  battery(+charge)` (`telemetry::derive_load`), recomputed on every CAN/grid
  update so it never uses a stale battery value.