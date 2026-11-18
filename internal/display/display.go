// Package display drives the module's e-ink panel (/dev/fb0) with a live
// status page: SOC, battery power + direction, grid, PV, faults, clock.
//
// e-ink specifics (the panel is electrophoretic, confirmed from the
// vendor strings "eink"/"ePD"):
//   - grayscale only (1/2/4/8 bpp); "colors" map to black/white, SOC
//     level is shown by text size + a battery outline, not color.
//   - every pixel write is durable but the CONTROLLER needs a flush to
//     physically update the panel. We flush via FBIOPUT_VSCREENINFO
//     (works on the common epd fb drivers); if the panel uses a
//     vendor-specific ioctl, adjust flush() on-device.
//   - full-screen refreshes flash. We draw into a shadow buffer and
//     write only changed bytes, refreshing at most every
//     refreshInterval and only when the visible content changed.
//
// Polarity: we assume 0x00 = white paper, max = black ink. If the panel
// shows inverted output, flip inkPolarity (const below) after on-device
// testing.
package display

import (
	"fmt"
	"os"
	"syscall"
	"time"
	"unsafe"

	"emaldo-local/internal/config"
	"emaldo-local/internal/telemetry"
)

const (
	fbGetVScreenInfo = 0x4600
	fbPutVScreenInfo = 0x4601
	fbGetFScreenInfo = 0x4602
	// inkPolarity: false => 0x00 white, maxval black. Flip if inverted.
	inkPolarityNormal = true
	refreshInterval   = 15 * time.Second
)

type fbVarScreenInfo struct {
	Xres, Yres, XresVirtual, YresVirtual uint32
	Xoffset, Yoffset                     uint32
	BitsPerPixel                         uint32
	Grayscale                            uint32
	Red, Green, Blue, Transp             [4]uint32
}

type fbFixScreenInfo struct {
	ID           [16]byte
	SmemStart    uint32
	SmemLen      uint32
	Type         uint32
	TypeAux      uint32
	Visual       uint32
	XpanStep     uint16
	YpanStep     uint16
	YwrapStep    uint16
	LineLength   uint32
	MMIOStart    uint32
	MMIOlen      uint32
	Accel        uint32
	Capabilities uint16
	_            [2]byte
	_2           [8]byte
}

// Renderer draws the status page onto the e-ink framebuffer.
type Renderer struct {
	Cfg   *config.Config
	Telem *telemetry.Store

	fb     *os.File
	fb0    []byte
	shadow []byte
	vi     fbVarScreenInfo
	fi     fbFixScreenInfo
	white  byte
	black  byte
	last   string // content fingerprint; skip unchanged redraws
}

func (r *Renderer) Start() {
	go r.loop()
}

func (r *Renderer) loop() {
	fb, err := os.OpenFile("/dev/fb0", os.O_RDWR, 0)
	if err != nil {
		r.Telem.AddEvent("warn", "display", "no /dev/fb0: "+err.Error())
		return
	}
	defer fb.Close()
	r.fb = fb
	if err := ioctl(fb.Fd(), fbGetVScreenInfo, unsafe.Pointer(&r.vi)); err != nil {
		r.Telem.AddEvent("warn", "display", "FBIOGET_VSCREENINFO: "+err.Error())
		return
	}
	if err := ioctl(fb.Fd(), fbGetFScreenInfo, unsafe.Pointer(&r.fi)); err != nil {
		r.Telem.AddEvent("warn", "display", "FBIOGET_FSCREENINFO: "+err.Error())
		return
	}
	size := int(r.fi.SmemLen)
	if size <= 0 || size > 64<<20 {
		r.Telem.AddEvent("warn", "display", fmt.Sprintf("bad fb size %d", size))
		return
	}
	buf, err := syscall.Mmap(int(fb.Fd()), 0, size,
		syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		r.Telem.AddEvent("warn", "display", "mmap: "+err.Error())
		return
	}
	defer syscall.Munmap(buf)
	r.fb0 = buf
	r.shadow = make([]byte, size)
	maxv := byte(0xFF >> (8 - min(r.vi.BitsPerPixel, 8)))
	if r.vi.BitsPerPixel > 8 {
		maxv = 0xFF // treated as gray8
	}
	if inkPolarityNormal {
		r.white, r.black = 0, maxv
	} else {
		r.white, r.black = maxv, 0
	}
	r.Telem.AddEvent("info", "display",
		fmt.Sprintf("e-ink fb %dx%d %dbpp gray", r.vi.Xres, r.vi.Yres, r.vi.BitsPerPixel))
	r.setBacklight(255)

	r.clearPaper() // one full clear at boot (the only full refresh)
	tick := time.NewTicker(refreshInterval)
	defer tick.Stop()
	r.render()
	for range tick.C {
		r.render()
	}
}

func ioctl(fd uintptr, req uint, arg unsafe.Pointer) error {
	_, _, errno := syscall.Syscall6(syscall.SYS_IOCTL, fd, uintptr(req),
		uintptr(arg), 0, 0, 0)
	if errno != 0 {
		return errno
	}
	return nil
}

func min(a, b uint32) uint32 {
	if a < b {
		return a
	}
	return b
}

func (r *Renderer) setBacklight(v int) {
	entries, err := os.ReadDir("/sys/class/backlight")
	if err != nil || len(entries) == 0 {
		return
	}
	base := "/sys/class/backlight/" + entries[0].Name()
	maxB, _ := os.ReadFile(base + "/max_brightness")
	mx := 255
	if len(maxB) > 0 {
		fmt.Sscanf(string(maxB), "%d", &mx)
	}
	if v > mx {
		v = mx
	}
	_ = os.WriteFile(base+"/brightness", []byte(fmt.Sprintf("%d", v)), 0o644)
}

// ---- e-ink primitives ----

// setInk marks a pixel black (ink) or white (paper) in the shadow only.
func (r *Renderer) setInk(x, y int, ink bool) {
	if x < 0 || y < 0 || x >= int(r.vi.Xres) || y >= int(r.vi.Yres) {
		return
	}
	bpp := r.vi.BitsPerPixel
	if bpp > 8 {
		bpp = 8
	}
	line := int(r.fi.LineLength)
	bit := x * int(bpp)
	o := y*line + bit/8
	if o < 0 || o >= len(r.shadow) {
		return
	}
	var val byte
	if ink {
		val = r.black
	} else {
		val = r.white
	}
	switch {
	case bpp == 1:
		mask := byte(0x80 >> (uint(bit) % 8))
		if ink {
			r.shadow[o] |= mask
		} else {
			r.shadow[o] &^= mask
		}
	case bpp == 8:
		r.shadow[o] = val
	default: // 2/4 bpp, nibble-aligned-ish: byte-granular best effort
		shift := uint(8 - bpp) - (uint(bit) % 8)
		mask := byte((1 << bpp) - 1)
		r.shadow[o] = (r.shadow[o] &^ (mask << shift)) | ((val & mask) << shift)
	}
}

// flushDiff copies changed shadow bytes to the panel and triggers a
// controller refresh. e-ink refreshes are slow; only dirty bytes are
// written. Best-effort: many epd fb drivers flush on FBIOPUT_VSCREENINFO.
func (r *Renderer) flushDiff() {
	dirty := false
	for i := 0; i < len(r.shadow); i++ {
		if r.shadow[i] != r.fb0[i] {
			r.fb0[i] = r.shadow[i]
			dirty = true
		}
	}
	if dirty {
		vi := r.vi // refresh trigger
		_ = ioctl(r.fb.Fd(), fbPutVScreenInfo, unsafe.Pointer(&vi))
	}
}

func (r *Renderer) clearPaper() {
	for i := range r.shadow {
		r.shadow[i] = r.white
	}
	r.flushDiff()
}

func (r *Renderer) text(x, y int, s string, ink, big bool) {
	scale := 1
	if big {
		scale = 2
	}
	cx := x
	for _, ch := range s {
		if ch == '\n' {
			y += 9 * scale
			cx = x
			continue
		}
		glyph := font8x8[ch]
		for row := 0; row < 8; row++ {
			bits := glyph[row]
			for col := 0; col < 8; col++ {
				on := bits&(1<<uint(7-col)) != 0
				for sy := 0; sy < scale; sy++ {
					for sx := 0; sx < scale; sx++ {
						if on || scale == 1 {
							r.setInk(cx+col*scale+sx, y+row*scale+sy, on)
						}
					}
				}
			}
		}
		cx += 6 * scale
	}
}

// batteryGlyph draws a simple battery outline with fill level at (x,y),
// 24x10 logical pixels, as the SOC visual (e-ink has no colors).
func (r *Renderer) batteryGlyph(x, y int, pct float64) {
	w, h := 22, 10
	for i := 0; i < w; i++ {
		r.setInk(x+i, y, true)
		r.setInk(x+i, y+h, true)
	}
	for j := 0; j <= h; j++ {
		r.setInk(x, y+j, true)
		r.setInk(x+w, y+j, true)
	}
	for j := 2; j < h-1; j += 2 { // terminal nub
		r.setInk(x+w+1, y+j, true)
		r.setInk(x+w+2, y+j, true)
	}
	fill := int(float64(w-2) * pct / 100.0)
	for i := 1; i <= fill; i++ {
		for j := 1; j < h; j++ {
			r.setInk(x+i, y+j, true)
		}
	}
}

func (r *Renderer) render() {
	st := r.Telem.Current()
	// fingerprint of everything visible; skip unchanged content
	fp := fmt.Sprintf("%.0f|%.0f|%.0f|%.0f|%v|%d|%s",
		st.Battery.SOC, st.Battery.PowerW, st.Grid.NetW, st.PV.PowerW,
		len(st.Faults) > 0, len(st.Inverters), st.UpdatedAt.Format("15:04"))
	if fp == r.last {
		return
	}
	r.last = fp

	// erase the text area (leave border-free full clear out; repaint band)
	for y := 0; y < int(r.vi.Yres); y++ {
		for x := 0; x < int(r.vi.Xres); x++ {
			r.setInk(x, y, false)
		}
	}

	w, h := int(r.vi.Xres), int(r.vi.Yres)
	scale2 := w >= 200 && h >= 120
	y := 4

	name := r.Cfg.Device.Name
	if name == "" {
		name = "emaldo-local"
	}
	r.text(4, y, name, true, false)
	y += 12

	// headline: battery glyph + big SOC
	r.batteryGlyph(4, y+2, st.Battery.SOC)
	r.text(34, y, fmt.Sprintf("%.0f%%", st.Battery.SOC), true, scale2)
	y += 16

	arrow := "->"
	if st.Battery.PowerW < 0 {
		arrow = "<-"
	}
	r.text(4, y, fmt.Sprintf("BAT %s %.0fW", arrow, absf(st.Battery.PowerW)), true, false)
	y += 11
	r.text(4, y, fmt.Sprintf("GRID %.0fW", st.Grid.NetW), true, false)
	y += 11
	r.text(4, y, fmt.Sprintf("PV   %.0fW", st.PV.PowerW), true, false)
	y += 11
	if len(st.Inverters) > 0 {
		inv := st.Inverters[0]
		state := "OFF"
		if inv.Running {
			state = "ON"
		}
		r.text(4, y, fmt.Sprintf("INV %s %.1fHz", state, inv.Frequency), true, false)
		y += 11
	}
	if len(st.Faults) > 0 {
		r.text(4, y, "FAULT "+st.Faults[0], true, false)
		y += 11
	}
	r.text(4, y, st.UpdatedAt.Format("15:04"), true, false)

	r.flushDiff()
}

func absf(f float64) float64 {
	if f < 0 {
		return -f
	}
	return f
}
