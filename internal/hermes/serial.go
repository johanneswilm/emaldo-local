// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Serial + Modbus RTU over a tty. Pure stdlib ioctl via syscall — no
// external termios dependency. Only Linux/ARM is a build target, but the
// code compiles on any Unix.
package hermes

import (
	"errors"
	"fmt"
	"io"
	"os"
	"syscall"
	"time"
	"unsafe"
)

const (
	rs485Enabled = 1 << 0
)

type termios struct {
	Iflag, Oflag, Cflag, Lflag uint32
	Line                       byte
	Cc                         [32]byte
	Ispeed, Ospeed             uint32
}

const (
	tcgets  = 0x5401
	tcsets  = 0x5402
	tiocgrs485 = 0x542E
	tiocsrs485 = 0x542F
)

// OpenSerial opens a tty at the given baud in 8N1 raw mode, enabling
// RS485 direction control when the driver supports it.
func OpenSerial(port string, baud int) (io.ReadWriteCloser, error) {
	f, err := os.OpenFile(port, os.O_RDWR|syscall.O_NOCTTY|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, err
	}
	var t termios
	if _, _, errno := syscall.Syscall6(syscall.SYS_IOCTL, f.Fd(), tcgets, uintptr(unsafe.Pointer(&t)), 0, 0, 0); errno != 0 {
		f.Close()
		return nil, fmt.Errorf("tcgetattr: %v", errno)
	}
	baudConst := baudToConst(baud)
	if baudConst == 0 {
		f.Close()
		return nil, fmt.Errorf("unsupported baud %d", baud)
	}
	t.Iflag &^= 0xFFFF // BRKINT | ICRNL | INPCK | ISTRIP | IXON
	t.Lflag &^= 0x8B00 // ECHO | ICANON | IEXTEN | ISIG
	t.Oflag &^= 0x1    // OPOST
	t.Cflag = 0x8B30   // CREAD | CS8 | CLOCAL | baud (8N1, no parity)
	t.Cc[6], t.Cc[5] = 1, 0
	if _, _, errno := syscall.Syscall6(syscall.SYS_IOCTL, f.Fd(), tcsets, uintptr(unsafe.Pointer(&t)), 0, 0, 0); errno != 0 {
		f.Close()
		return nil, fmt.Errorf("tcsetattr: %v", errno)
	}
	// Best-effort RS485.
	var rs struct {
		flags      uint32
		delayRtsBeforeSend, delayRtsAfterSend uint32
		padding    [4]uint32
	}
	rs.flags = rs485Enabled
	if _, _, errno := syscall.Syscall6(syscall.SYS_IOCTL, f.Fd(), tiocsrs485, uintptr(unsafe.Pointer(&rs)), 0, 0, 0); errno != 0 {
		// Not RS485-capable — fine, plain UART works too.
	}
	if err := syscall.SetNonblock(int(f.Fd()), false); err != nil {
		f.Close()
		return nil, err
	}
	return f, nil
}

func baudToConst(b int) uint32 {
	switch b {
	case 9600:
		return 0x0D
	case 19200:
		return 0x0E
	case 38400:
		return 0x0F
	case 57600:
		return 0x1001
	case 115200:
		return 0x1002
	}
	return 0
}

// Modbus RTU framing.
const (
	fcReadHolding = 0x03
)

// ReadHoldingRegisters performs a Modbus RTU FC03 read of `count`
// registers starting at `addr` and returns the raw payload (count*2
// bytes). Every register is big-endian, and multi-register values put the
// high word first. The vendor firmware only ever uses FC03/06/16; FC04
// (input registers) is never issued (docs/REGISTERS.md).
func ReadHoldingRegisters(rw io.ReadWriter, slave byte, addr uint16, count uint16) ([]byte, error) {
	if count == 0 || count > 125 {
		return nil, fmt.Errorf("modbus: invalid quantity %d", count)
	}
	req := []byte{slave, fcReadHolding, byte(addr >> 8), byte(addr), byte(count >> 8), byte(count)}
	req = append(req, crc16(req)...)
	if _, err := rw.Write(req); err != nil {
		return nil, err
	}
	if d, ok := rw.(interface{ SetReadDeadline(time.Time) error }); ok {
		d.SetReadDeadline(time.Now().Add(2 * time.Second))
	}
	// Read the 3-byte header first so an exception reply (5 bytes total)
	// is reported straight away instead of waiting for the full length.
	hdr := make([]byte, 3)
	if _, err := io.ReadFull(rw, hdr); err != nil {
		return nil, err
	}
	if hdr[0] != slave {
		return nil, errors.New("modbus: wrong slave in response")
	}
	if hdr[1] == fcReadHolding|0x80 {
		tail := make([]byte, 2)
		if _, err := io.ReadFull(rw, tail); err != nil {
			return nil, err
		}
		if !equal(tail, crc16(hdr)) {
			return nil, errors.New("modbus: crc mismatch")
		}
		return nil, fmt.Errorf("modbus exception 0x%02x", hdr[2])
	}
	if hdr[1] != fcReadHolding {
		return nil, fmt.Errorf("modbus: unexpected function 0x%02x", hdr[1])
	}
	n := int(hdr[2])
	if n != int(count)*2 {
		return nil, errors.New("modbus: bad length")
	}
	rest := make([]byte, n+2)
	if _, err := io.ReadFull(rw, rest); err != nil {
		return nil, err
	}
	frame := append(hdr, rest[:n]...)
	if !equal(rest[n:], crc16(frame)) {
		return nil, errors.New("modbus: crc mismatch")
	}
	return rest[:n], nil
}

func crc16(b []byte) []byte {
	var crc uint16 = 0xFFFF
	for _, c := range b {
		crc ^= uint16(c)
		for i := 0; i < 8; i++ {
			if crc&1 != 0 {
				crc = (crc >> 1) ^ 0xA001
			} else {
				crc >>= 1
			}
		}
	}
	return []byte{byte(crc), byte(crc >> 8)}
}

func equal(a, b []byte) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
