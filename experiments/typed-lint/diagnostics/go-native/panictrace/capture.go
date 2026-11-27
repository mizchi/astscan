// Package panictrace captures a recovered Go panic without formatting a stack.
package panictrace

import (
	"fmt"
	"runtime"
)

type Location struct {
	File string `json:"file"`
	Line int    `json:"line"`
}

type Frame struct {
	FunctionName string    `json:"functionName,omitempty"`
	Location     *Location `json:"location,omitempty"`
}

type Trace struct {
	Frames []Frame `json:"frames"`
}

type Report struct {
	Message string  `json:"message"`
	Traces  []Trace `json:"traces"`
}

// Capture is called in the deferred function that recovered the panic.
// It captures the current goroutine, beginning at the panic site if present.
func Capture(value any) Report {
	pcs := make([]uintptr, 32)
	var n int
	for {
		n = runtime.Callers(2, pcs)
		if n < len(pcs) {
			break
		}
		pcs = make([]uintptr, 2*len(pcs))
	}
	iterator := runtime.CallersFrames(pcs[:n])
	frames := []Frame{}
	for n != 0 {
		frame, more := iterator.Next()
		if frame.Function == "runtime.gopanic" {
			frames = frames[:0]
		} else {
			item := Frame{FunctionName: frame.Function}
			if frame.File != "" && frame.Line > 0 {
				item.Location = &Location{frame.File, frame.Line}
			}
			frames = append(frames, item)
		}
		if !more {
			break
		}
	}
	return Report{Message: fmt.Sprint(value), Traces: []Trace{{frames}}}
}
