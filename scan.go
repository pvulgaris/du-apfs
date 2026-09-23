package main

import (
	"encoding/binary"
	"errors"
	"fmt"
	"math/bits"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"sync"
	"syscall"
	"unsafe"
)

const (
	log2physExt  = 65
	ufCompressed = 0x20
)

type extent struct {
	start int64
	end   int64
}

type extentList struct {
	// Most files have one extent, so keep it inline to avoid a per-file allocation.
	first extent
	rest  []extent
	count int
}

func (e *extentList) add(r extent) {
	if e.count == 0 {
		e.first = r
	} else {
		e.rest = append(e.rest, r)
	}
	e.count++
}

type event struct {
	dev   uint64
	pos   int64
	delta int8
	node  int32
}

type bitmapSet struct {
	words    map[uint64][]uint64
	chunks   map[uint64]map[uint64]*[64]uint64
	sparse   bool
	physical int64
}

type node struct {
	path       string
	parent     int32
	depth      int
	operand    int
	dir        bool
	root       bool
	allocated  int64
	physical   int64
	unmeasured int64
	active     int32
	opened     int64
}

type scanError struct {
	Path   string `json:"path"`
	Reason string `json:"reason"`
}

type scanner struct {
	nodes                 []node
	roots                 []int32
	events                []event
	bitmaps               []bitmapSet
	errors                []scanError
	errorCount            int
	unreadableDirectories int
	unreadablePaths       []string
	skipped               []string
	files                 int64
	blockSize             int64
	allFiles              bool
	oneFS                 bool
	maxDepth              int
	rootPaths             []string
	bitmapMode            bool
	workers               int
}

type pending struct {
	path    string
	parent  int32
	depth   int
	operand int
	rootDev uint64
	root    bool
}

type fileJob struct {
	path      string
	stat      syscall.Stat_t
	owner     int32
	blockSize int64
}

type fileResult struct {
	job    fileJob
	ranges extentList
	extra  int64
	err    error
}

func newScanner(allFiles, oneFS bool, maxDepth int) *scanner {
	workers := runtime.NumCPU()
	if workers > 3 {
		workers = 3
	}
	return &scanner{
		nodes:           []node{{parent: -1, dir: true}},
		errors:          []scanError{},
		skipped:         []string{},
		unreadablePaths: []string{},
		blockSize:       4096,
		allFiles:        allFiles,
		oneFS:           oneFS,
		maxDepth:        maxDepth,
		workers:         workers,
	}
}

func (s *scanner) issue(path string, err error) {
	s.errorCount++
	if len(s.errors) < 100 {
		s.errors = append(s.errors, scanError{path, err.Error()})
	}
}

func (s *scanner) addNode(path string, parent int32, depth, operand int, dir, root bool, allocated int64) int32 {
	id := int32(len(s.nodes))
	s.nodes = append(s.nodes, node{
		path: path, parent: parent, depth: depth, operand: operand, dir: dir, root: root,
		allocated: allocated,
	})
	if root {
		s.roots = append(s.roots, id)
	}
	if s.bitmapMode {
		// A shallow child can span a large disk address range while using few blocks.
		s.bitmaps = append(s.bitmaps, bitmapSet{sparse: s.maxDepth == 1 && depth == 1})
	}
	return id
}

func lstat(path string) (syscall.Stat_t, error) {
	var st syscall.Stat_t
	for {
		err := syscall.Lstat(path, &st)
		if err == syscall.EINTR {
			continue
		}
		return st, err
	}
}

func readDirNames(path string) ([]string, error) {
	dir, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	names, readErr := dir.Readdirnames(-1)
	closeErr := dir.Close()
	if readErr != nil {
		return nil, readErr
	}
	return names, closeErr
}

func (s *scanner) scan(paths []string) error {
	stack := make([]pending, 0, len(paths))
	s.rootPaths = make([]string, len(paths))
	s.bitmapMode = s.maxDepth >= 0 && s.maxDepth <= 1 && !s.allFiles
	if s.bitmapMode {
		s.bitmaps = make([]bitmapSet, len(s.nodes))
	}
	for i := len(paths) - 1; i >= 0; i-- {
		path, err := filepath.Abs(paths[i])
		if err != nil {
			return err
		}
		st, err := lstat(path)
		if err != nil {
			return err
		}
		var fs syscall.Statfs_t
		if err := syscall.Statfs(path, &fs); err != nil {
			return err
		}
		fsType := make([]byte, 0, len(fs.Fstypename))
		for _, letter := range fs.Fstypename {
			if letter == 0 {
				break
			}
			fsType = append(fsType, byte(letter))
		}
		if string(fsType) != "apfs" {
			return fmt.Errorf("%s is not on APFS", path)
		}
		if s.blockSize == 4096 {
			s.blockSize = int64(fs.Bsize)
		}
		s.rootPaths[i] = path
		stack = append(stack, pending{
			path: path, parent: 0, depth: 0, operand: i,
			rootDev: uint64(st.Dev), root: true,
		})
	}
	jobs := make(chan fileJob, 2048)
	results := make(chan fileResult, 2048)
	var group sync.WaitGroup
	for i := 0; i < s.workers; i++ {
		group.Add(1)
		go func() {
			defer group.Done()
			for job := range jobs {
				ranges, extra, err := fileExtents(job.path, &job.stat, job.blockSize)
				results <- fileResult{job, ranges, extra, err}
			}
		}()
	}
	outstanding := 0
	for len(stack) > 0 {
		// The walker owns all accounting; workers only return immutable maps.
		select {
		case result := <-results:
			s.consumeResult(result)
			outstanding--
		default:
		}
		entry := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		st, err := lstat(entry.path)
		if err != nil {
			s.issue(entry.path, err)
			continue
		}
		if s.oneFS && uint64(st.Dev) != entry.rootDev {
			s.skipped = append(s.skipped, entry.path)
			continue
		}
		if entry.path == "/System/Volumes/Data" && s.rootPaths[entry.operand] == "/" {
			// The Data volume is already visible through root firmlinks.
			s.skipped = append(s.skipped, entry.path)
			continue
		}
		allocated := st.Blocks * 512
		if st.Mode&syscall.S_IFMT == syscall.S_IFDIR {
			id := entry.parent
			if entry.root || s.maxDepth < 0 || entry.depth <= s.maxDepth {
				id = s.addNode(entry.path, entry.parent, entry.depth, entry.operand, true, entry.root, allocated)
				s.nodes[id].unmeasured = allocated
			} else {
				s.nodes[id].allocated += allocated
				s.nodes[id].unmeasured += allocated
			}
			children, err := readDirNames(entry.path)
			if err != nil {
				s.issue(entry.path, err)
				s.unreadableDirectories++
				if len(s.unreadablePaths) < 100 {
					s.unreadablePaths = append(s.unreadablePaths, entry.path)
				}
				continue
			}
			for i := len(children) - 1; i >= 0; i-- {
				stack = append(stack, pending{
					path:   filepath.Join(entry.path, children[i]),
					parent: id, depth: entry.depth + 1, operand: entry.operand,
					rootDev: entry.rootDev,
				})
			}
			continue
		}
		s.files++
		if s.files%500000 == 0 {
			fmt.Fprintf(os.Stderr, "Scanned %d files...\n", s.files)
		}
		owner := entry.parent
		if entry.root || s.allFiles {
			owner = s.addNode(entry.path, entry.parent, entry.depth, entry.operand, false, entry.root, allocated)
		} else {
			s.nodes[owner].allocated += allocated
		}
		if st.Mode&syscall.S_IFMT == syscall.S_IFREG && allocated > 0 {
			job := fileJob{entry.path, st, owner, s.blockSize}
			queued := false
			for !queued {
				select {
				case jobs <- job:
					outstanding++
					queued = true
				case result := <-results:
					s.consumeResult(result)
					outstanding--
				}
			}
		} else {
			s.nodes[owner].unmeasured += allocated
		}
	}
	close(jobs)
	for outstanding > 0 {
		s.consumeResult(<-results)
		outstanding--
	}
	group.Wait()
	s.calculatePhysical()
	for i := len(s.nodes) - 1; i > 0; i-- {
		current := &s.nodes[i]
		parent := &s.nodes[current.parent]
		parent.allocated += current.allocated
		parent.unmeasured += current.unmeasured
	}
	return nil
}

func (s *scanner) consumeResult(result fileResult) {
	job := result.job
	if result.err != nil {
		s.issue(job.path, result.err)
		s.nodes[job.owner].unmeasured += job.stat.Blocks * 512
		return
	}
	if result.extra > 0 {
		s.issue(job.path, fmt.Errorf("%d allocated bytes outside mapped forks", result.extra))
		s.nodes[job.owner].unmeasured += result.extra
	}
	if result.ranges.count > 0 {
		s.addRange(uint64(job.stat.Dev), job.owner, result.ranges.first)
	}
	for _, r := range result.ranges.rest {
		s.addRange(uint64(job.stat.Dev), job.owner, r)
	}
}

func (s *scanner) addRange(dev uint64, owner int32, r extent) {
	if s.bitmapMode {
		for id := owner; id >= 0; id = s.nodes[id].parent {
			if id == 0 && len(s.rootPaths) == 1 {
				continue
			}
			s.bitmaps[id].addRange(dev, r, s.blockSize)
		}
		return
	}
	s.events = append(s.events,
		event{dev, r.start, 1, owner},
		event{dev, r.end, -1, owner},
	)
}

func (b *bitmapSet) addRange(dev uint64, r extent, blockSize int64) {
	first := uint64(r.start / blockSize)
	last := uint64((r.end - 1) / blockSize)
	firstWord, lastWord := first/64, last/64
	if b.sparse {
		if b.chunks == nil {
			b.chunks = make(map[uint64]map[uint64]*[64]uint64)
		}
		byDevice := b.chunks[dev]
		if byDevice == nil {
			byDevice = make(map[uint64]*[64]uint64)
			b.chunks[dev] = byDevice
		}
		for i := firstWord; i <= lastWord; i++ {
			chunkID := i / 64
			chunk := byDevice[chunkID]
			if chunk == nil {
				chunk = new([64]uint64)
				byDevice[chunkID] = chunk
			}
			low, high := uint64(0), uint64(63)
			if i == firstWord {
				low = first % 64
			}
			if i == lastWord {
				high = last % 64
			}
			mask := (^uint64(0) << low) & (^uint64(0) >> (63 - high))
			word := &chunk[i%64]
			b.physical += int64(bits.OnesCount64(mask&^*word)) * blockSize
			*word |= mask
		}
		return
	}
	if b.words == nil {
		b.words = make(map[uint64][]uint64)
	}
	words := b.words[dev]
	if lastWord >= uint64(len(words)) {
		needed := int(lastWord + 1)
		if needed > cap(words) {
			capacity := cap(words) + cap(words)/2 + 1024
			if capacity < needed {
				capacity = needed
			}
			grown := make([]uint64, needed, capacity)
			copy(grown, words)
			words = grown
		} else {
			words = words[:needed]
		}
		b.words[dev] = words
	}
	for i := firstWord; i <= lastWord; i++ {
		low, high := uint64(0), uint64(63)
		if i == firstWord {
			low = first % 64
		}
		if i == lastWord {
			high = last % 64
		}
		mask := (^uint64(0) << low) & (^uint64(0) >> (63 - high))
		newBits := mask &^ words[i]
		b.physical += int64(bits.OnesCount64(newBits)) * blockSize
		words[i] |= mask
	}
}

func (s *scanner) calculatePhysical() {
	if s.bitmapMode {
		for i := range s.bitmaps {
			s.nodes[i].physical = s.bitmaps[i].physical
		}
		if len(s.rootPaths) == 1 {
			s.nodes[0].physical = s.nodes[s.roots[0]].physical
		}
		return
	}
	slices.SortFunc(s.events, func(a, b event) int {
		if a.dev != b.dev {
			if a.dev < b.dev {
				return -1
			}
			return 1
		}
		if a.pos != b.pos {
			if a.pos < b.pos {
				return -1
			}
			return 1
		}
		return int(a.delta) - int(b.delta)
	})
	for _, e := range s.events {
		for id := e.node; id >= 0; id = s.nodes[id].parent {
			n := &s.nodes[id]
			if e.delta > 0 && n.active == 0 {
				n.opened = e.pos
			}
			n.active += int32(e.delta)
			if e.delta < 0 && n.active == 0 {
				n.physical += e.pos - n.opened
			}
		}
	}
}

func sameFile(a, b *syscall.Stat_t) bool {
	return a.Dev == b.Dev && a.Ino == b.Ino && a.Blocks == b.Blocks &&
		a.Mtimespec == b.Mtimespec
}

func mapFork(fd int, size int64, blockSize int64) (extentList, int64, error) {
	var ranges extentList
	var mapped int64
	for offset := int64(0); offset < size; {
		var request [20]byte // Darwin's log2phys struct is packed to four bytes.
		binary.LittleEndian.PutUint64(request[4:12], uint64(size-offset))
		binary.LittleEndian.PutUint64(request[12:20], uint64(offset))
		_, _, errno := syscall.Syscall(syscall.SYS_FCNTL, uintptr(fd), log2physExt,
			uintptr(unsafe.Pointer(&request[0])))
		if errno != 0 {
			return extentList{}, 0, errno
		}
		length := int64(binary.LittleEndian.Uint64(request[4:12]))
		physical := int64(binary.LittleEndian.Uint64(request[12:20]))
		if length <= 0 || length > size-offset {
			return extentList{}, 0, errors.New("invalid extent returned by macOS")
		}
		if physical >= 0 {
			start := physical / blockSize * blockSize
			end := (physical + length + blockSize - 1) / blockSize * blockSize
			ranges.add(extent{start, end})
			mapped += end - start
		}
		offset += length
	}
	return ranges, mapped, nil
}

func fileExtents(path string, expected *syscall.Stat_t, blockSize int64) (extentList, int64, error) {
	fd, err := syscall.Open(path, syscall.O_RDONLY|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return extentList{}, 0, err
	}
	defer syscall.Close(fd)
	var current syscall.Stat_t
	if err := syscall.Fstat(fd, &current); err != nil {
		return extentList{}, 0, err
	}
	if !sameFile(&current, expected) || current.Size != expected.Size {
		return extentList{}, 0, errors.New("file changed during scan")
	}
	ranges, mapped, err := mapFork(fd, current.Size, blockSize)
	if errors.Is(err, syscall.ENOTSUP) && current.Flags&ufCompressed != 0 {
		resource, openErr := syscall.Open(path+"/..namedfork/rsrc", syscall.O_RDONLY|syscall.O_NOFOLLOW, 0)
		if openErr != nil {
			return extentList{}, 0, openErr
		}
		defer syscall.Close(resource)
		var fork syscall.Stat_t
		if err := syscall.Fstat(resource, &fork); err != nil {
			return extentList{}, 0, err
		}
		if !sameFile(&fork, &current) {
			return extentList{}, 0, errors.New("file changed during scan")
		}
		ranges, mapped, err = mapFork(resource, fork.Size, blockSize)
	}
	if err != nil {
		return extentList{}, 0, err
	}
	var after syscall.Stat_t
	if err := syscall.Fstat(fd, &after); err != nil {
		return extentList{}, 0, err
	}
	if !sameFile(&after, &current) {
		return extentList{}, 0, errors.New("file changed during scan")
	}
	allocated := current.Blocks * 512
	if mapped > allocated {
		return extentList{}, 0, errors.New("extent map exceeds allocated blocks")
	}
	return ranges, allocated - mapped, nil
}
