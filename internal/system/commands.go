package system

import (
	"errors"
	"os/exec"
	"runtime"
)

const (
	WINDOWS = "windows"
	MACOS   = "darwin"
	LINUX   = "linux"
)

func GetShutdownCommand() (*exec.Cmd, error) {
	switch runtime.GOOS {
	case WINDOWS:
		return exec.Command("shutdown", "/s"), nil
	case MACOS:
		return exec.Command("shutdown", "-h", "now"), nil
	case LINUX:
		return exec.Command("systemctl", "poweroff", "--ignore-inhibitors"), nil
	default:
		return nil, errors.New(runtime.GOOS + " does not support shutdown")
	}
}

func GetRebootCommand() (*exec.Cmd, error) {
	switch runtime.GOOS {
	case WINDOWS:
		return exec.Command("shutdown", "/r"), nil
	case MACOS:
		return exec.Command("reboot"), nil
	case LINUX:
		return exec.Command("systemctl", "reboot", "--ignore-inhibitors"), nil
	default:
		return nil, errors.New(runtime.GOOS + " does not support reboot")
	}
}
