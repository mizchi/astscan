package main

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/scanner"
	"go/token"
	"os"
	"regexp"
	"strconv"
	"strings"
	"sync"

	"golang.org/x/tools/go/packages"
)

type byteRange struct {
	Start int `json:"start"`
	End   int `json:"end"`
}

type location struct {
	File   string     `json:"file"`
	Line   int        `json:"line"`
	Column int        `json:"column,omitempty"`
	Bytes  *byteRange `json:"bytes,omitempty"`
}

type evidence struct {
	Kind      string     `json:"kind"`
	Message   string     `json:"message"`
	Locations []location `json:"locations"`
}

type diagnostic struct {
	Source    string     `json:"source"`
	Phase     string     `json:"phase"`
	Severity  string     `json:"severity"`
	Message   string     `json:"message"`
	Locations []location `json:"locations"`
	Evidence  []evidence `json:"evidence"`
}

func at(pos token.Position) []location {
	if !pos.IsValid() {
		return []location{}
	}
	return []location{{File: pos.Filename, Line: pos.Line, Column: pos.Column, Bytes: &byteRange{pos.Offset, pos.Offset}}}
}

var listPosition = regexp.MustCompile(`^(.+?):([0-9]+)(?::([0-9]+))?$`)

// Package discovery exposes a position string in a dedicated field.
func packageErrorAt(pos string) []location {
	parts := listPosition.FindStringSubmatch(pos)
	if parts == nil {
		return []location{}
	}
	line, _ := strconv.Atoi(parts[2])
	column, _ := strconv.Atoi(parts[3])
	return []location{{File: parts[1], Line: line, Column: column}}
}

func report(message string, locations []location) diagnostic {
	return diagnostic{"go", "compile", "error", message, locations, []evidence{}}
}

func check(dir string, patterns []string) ([]diagnostic, error) {
	var mu sync.Mutex
	parsed := make(map[string][]diagnostic)
	type errorKey struct{ pos, message string }
	structured := make(map[errorKey]bool)
	loaded, err := packages.Load(&packages.Config{
		Dir:  dir,
		Mode: packages.LoadSyntax | packages.NeedModule,
		ParseFile: func(fset *token.FileSet, filename string, src []byte) (*ast.File, error) {
			file, err := parser.ParseFile(fset, filename, src, parser.SkipObjectResolution)
			if parseErrors, ok := err.(scanner.ErrorList); ok && file != nil {
				tokenFile := fset.File(file.FileStart)
				mu.Lock()
				for _, failure := range parseErrors {
					physical := tokenFile.PositionFor(tokenFile.Pos(failure.Pos.Offset), false)
					parsed[filename] = append(parsed[filename], report(failure.Msg, at(physical)))
					structured[errorKey{failure.Pos.String(), failure.Msg}] = true
				}
				mu.Unlock()
			}
			return file, err
		},
	}, patterns...)
	if err != nil {
		return nil, err
	}

	findings := []diagnostic{}
	seenFiles := make(map[string]bool)
	for _, pkg := range loaded {
		for _, path := range pkg.CompiledGoFiles {
			if !seenFiles[path] {
				findings = append(findings, parsed[path]...)
				seenFiles[path] = true
			}
		}
		// go/types supplies a separate error with a real position for each
		// secondary row. The callback represents those rows with a leading tab.
		start := len(findings)
		for _, failure := range pkg.TypeErrors {
			pos := failure.Fset.PositionFor(failure.Pos, false)
			locations := at(pos)
			if strings.HasPrefix(failure.Msg, "\t") && len(findings) > start {
				current := &findings[len(findings)-1]
				current.Evidence = append(current.Evidence, evidence{"related", strings.TrimPrefix(failure.Msg, "\t"), locations})
			} else {
				findings = append(findings, report(failure.Msg, locations))
			}
		}
		for _, failure := range pkg.Errors {
			if failure.Kind != packages.TypeError && !structured[errorKey{failure.Pos, failure.Msg}] {
				findings = append(findings, report(failure.Msg, packageErrorAt(failure.Pos)))
			}
		}
	}
	return findings, nil
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: check <project-dir> <package>...")
		os.Exit(2)
	}
	findings, err := check(os.Args[1], os.Args[2:])
	if err == nil {
		err = json.NewEncoder(os.Stdout).Encode(findings)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
