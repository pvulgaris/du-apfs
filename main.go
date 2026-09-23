package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

var errHelp = errors.New("help requested")

type options struct {
	all       bool
	summary   bool
	human     bool
	total     bool
	oneFS     bool
	json      bool
	null      bool
	depth     int
	blockSize int64
	paths     []string
}

func usage() string {
	return `usage: du-apfs [-achkmgsx0] [-d depth] [-B blocksize] [--json] [path ...]

APFS-aware physical usage. Common du flags are supported:
  -a  include files                 -s  show only each operand
  -d  maximum directory depth      -h  human-readable sizes
  -k/-m/-g  KiB/MiB/GiB units       -B  block size (for example -B1)
  -c  print a distinct total       -x  stay on each operand's filesystem
  -0  NUL-terminate rows           --json  structured byte counts
`
}

func parseSize(value string) (int64, error) {
	if value == "" {
		return 0, fmt.Errorf("empty block size")
	}
	multiplier := int64(1)
	suffix := strings.ToUpper(value[len(value)-1:])
	switch suffix {
	case "K", "M", "G", "T":
		multiplier = 1 << (10 * (strings.Index("KMGT", suffix) + 1))
		value = value[:len(value)-1]
	}
	n, err := strconv.ParseInt(value, 10, 64)
	if err != nil || n <= 0 || n > (1<<63-1)/multiplier {
		return 0, fmt.Errorf("invalid block size")
	}
	return n * multiplier, nil
}

func parseArgs(args []string) (options, error) {
	o := options{depth: -1, blockSize: 512}
	operandMode := false
	for i := 0; i < len(args); i++ {
		arg := args[i]
		if operandMode || arg == "-" || !strings.HasPrefix(arg, "-") {
			o.paths = append(o.paths, arg)
			continue
		}
		if arg == "--" {
			operandMode = true
			continue
		}
		if strings.HasPrefix(arg, "--") {
			name, value, hasValue := strings.Cut(arg[2:], "=")
			switch name {
			case "help":
				return o, errHelp
			case "json":
				o.json = true
			case "human-readable":
				o.human = true
			case "summarize":
				o.summary = true
			case "all":
				o.all = true
			case "total":
				o.total = true
			case "one-file-system":
				o.oneFS = true
			case "null":
				o.null = true
			case "max-depth", "block-size":
				if !hasValue {
					i++
					if i >= len(args) {
						return o, fmt.Errorf("--%s needs a value", name)
					}
					value = args[i]
				}
				if name == "max-depth" {
					depth, err := strconv.Atoi(value)
					if err != nil || depth < 0 {
						return o, fmt.Errorf("invalid depth")
					}
					o.depth = depth
				} else {
					size, err := parseSize(value)
					if err != nil {
						return o, err
					}
					o.blockSize, o.human = size, false
				}
			default:
				return o, fmt.Errorf("unsupported option --%s", name)
			}
			continue
		}
		for j := 1; j < len(arg); j++ {
			switch arg[j] {
			case 'a':
				o.all = true
			case 's':
				o.summary = true
			case 'h':
				o.human = true
			case 'c':
				o.total = true
			case 'x':
				o.oneFS = true
			case '0':
				o.null = true
			case 'k', 'm', 'g':
				o.blockSize = 1 << (10 * (strings.IndexByte("kmg", arg[j]) + 1))
				o.human = false
			case 'd', 'B':
				value := arg[j+1:]
				if value == "" {
					i++
					if i >= len(args) {
						return o, fmt.Errorf("-%c needs a value", arg[j])
					}
					value = args[i]
				}
				if arg[j] == 'd' {
					depth, err := strconv.Atoi(value)
					if err != nil || depth < 0 {
						return o, fmt.Errorf("invalid depth")
					}
					o.depth = depth
				} else {
					size, err := parseSize(value)
					if err != nil {
						return o, err
					}
					o.blockSize, o.human = size, false
				}
				j = len(arg)
			default:
				return o, fmt.Errorf("unsupported option -%c", arg[j])
			}
		}
	}
	if o.summary && o.all {
		return o, fmt.Errorf("-a and -s cannot be combined")
	}
	if len(o.paths) == 0 {
		o.paths = []string{"."}
	}
	return o, nil
}

func humanSize(bytes int64) string {
	if bytes == 0 {
		return "0B"
	}
	value := float64(bytes)
	for _, unit := range []string{"B", "K", "M", "G", "T"} {
		if value < 1024 || unit == "T" {
			if unit == "B" {
				return fmt.Sprintf("%dB", bytes)
			}
			return fmt.Sprintf("%.1f%s", value, unit)
		}
		value /= 1024
	}
	return "0B"
}

func formattedSize(bytes int64, o options) string {
	if o.human {
		return humanSize(bytes)
	}
	return strconv.FormatInt((bytes+o.blockSize-1)/o.blockSize, 10)
}

type outputEntry struct {
	Path            string `json:"path"`
	DisplayPath     string `json:"display_path"`
	Type            string `json:"type"`
	PhysicalBytes   int64  `json:"physical_bytes"`
	AllocatedBytes  int64  `json:"allocated_bytes"`
	UnmeasuredBytes int64  `json:"unmeasured_bytes"`
}

func displayPath(path, root, operand string) string {
	if path == root {
		return operand
	}
	rel, err := filepath.Rel(root, path)
	if err != nil {
		return path
	}
	if strings.HasSuffix(operand, string(os.PathSeparator)) {
		return operand + rel
	}
	return operand + string(os.PathSeparator) + rel
}

func output(s *scanner, o options, elapsed time.Duration) error {
	entries := make([]outputEntry, 0)
	absoluteRoots := make([]string, len(o.paths))
	for i, path := range o.paths {
		absoluteRoots[i], _ = filepath.Abs(path)
	}
	buckets := make([][]int32, len(o.paths))
	for i := len(s.nodes) - 1; i > 0; i-- {
		n := &s.nodes[i]
		if !n.root && (o.summary || (!n.dir && !o.all) || (o.depth >= 0 && n.depth > o.depth)) {
			continue
		}
		buckets[n.operand] = append(buckets[n.operand], int32(i))
	}
	for operand, ids := range buckets {
		for _, id := range ids {
			n := &s.nodes[id]
			kind := "file"
			if n.dir {
				kind = "directory"
			}
			entries = append(entries, outputEntry{
				Path: n.path, DisplayPath: displayPath(n.path, absoluteRoots[operand], o.paths[operand]),
				Type: kind, PhysicalBytes: n.physical,
				AllocatedBytes: n.allocated, UnmeasuredBytes: n.unmeasured,
			})
		}
	}
	if o.json {
		var fs syscall.Statfs_t
		_ = syscall.Statfs(absoluteRoots[0], &fs)
		result := map[string]any{
			"schema_version":       1,
			"entries":              entries,
			"total_physical_bytes": s.nodes[0].physical,
			"filesystem": map[string]uint64{
				"capacity_bytes": fs.Blocks * uint64(fs.Bsize),
				"free_bytes":     fs.Bfree * uint64(fs.Bsize),
				"used_bytes":     (fs.Blocks - fs.Bfree) * uint64(fs.Bsize),
			},
			"coverage": map[string]any{
				"complete":                   s.errorCount == 0 && s.nodes[0].unmeasured == 0,
				"error_count":                s.errorCount,
				"errors":                     s.errors,
				"errors_truncated":           s.errorCount > len(s.errors),
				"unmeasured_bytes":           s.nodes[0].unmeasured,
				"unreadable_directories":     s.unreadableDirectories,
				"unreadable_directory_paths": s.unreadablePaths,
				"skipped":                    s.skipped,
			},
			"files_scanned":   s.files,
			"elapsed_seconds": elapsed.Seconds(),
		}
		encoder := json.NewEncoder(os.Stdout)
		return encoder.Encode(result)
	}
	end := "\n"
	if o.null {
		end = "\x00"
	}
	for _, entry := range entries {
		fmt.Fprintf(os.Stdout, "%s\t%s%s", formattedSize(entry.PhysicalBytes, o), entry.DisplayPath, end)
	}
	if o.total {
		fmt.Fprintf(os.Stdout, "%s\ttotal%s", formattedSize(s.nodes[0].physical, o), end)
	}
	return nil
}

func main() {
	o, err := parseArgs(os.Args[1:])
	if errors.Is(err, errHelp) {
		fmt.Print(usage())
		return
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	started := time.Now()
	maxDepth := o.depth
	if o.summary {
		maxDepth = 0
	}
	if o.all {
		maxDepth = -1
	}
	s := newScanner(o.all, o.oneFS, maxDepth)
	if err := s.scan(o.paths); err != nil {
		fmt.Fprintln(os.Stderr, "du-apfs:", err)
		os.Exit(1)
	}
	if err := output(s, o, time.Since(started)); err != nil {
		fmt.Fprintln(os.Stderr, "du-apfs:", err)
		os.Exit(1)
	}
	if s.errorCount > 0 || s.nodes[0].unmeasured > 0 {
		fmt.Fprintf(os.Stderr, "du-apfs: incomplete: %d errors, %d unmeasured bytes\n",
			s.errorCount, s.nodes[0].unmeasured)
		os.Exit(1)
	}
}
