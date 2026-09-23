package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

func clone(t *testing.T, source, target string) {
	t.Helper()
	if output, err := exec.Command("cp", "-c", source, target).CombinedOutput(); err != nil {
		t.Fatalf("clone: %v: %s", err, output)
	}
}

func TestSharedExtentsAcrossDirectories(t *testing.T) {
	root := t.TempDir()
	left := filepath.Join(root, "left")
	right := filepath.Join(root, "right")
	for _, path := range []string{left, right} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	source := filepath.Join(left, "source")
	if err := os.WriteFile(source, make([]byte, 256*1024), 0600); err != nil {
		t.Fatal(err)
	}
	clone(t, source, filepath.Join(right, "full"))
	partial := filepath.Join(right, "partial")
	clone(t, source, partial)
	file, err := os.OpenFile(partial, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.Write([]byte("changed")); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(source, filepath.Join(right, "hardlink")); err != nil {
		t.Fatal(err)
	}
	s := newScanner(false, false, -1)
	if err := s.scan([]string{root}); err != nil {
		t.Fatal(err)
	}
	if s.errorCount != 0 {
		t.Fatalf("unexpected scan errors: %+v", s.errors)
	}
	var leftBytes, rightBytes int64
	for _, n := range s.nodes {
		switch n.path {
		case left:
			leftBytes = n.physical
		case right:
			rightBytes = n.physical
		}
	}
	whole := s.nodes[s.roots[0]].physical
	if whole <= leftBytes || whole >= leftBytes+rightBytes {
		t.Fatalf("shared blocks not represented: whole=%d left=%d right=%d", whole, leftBytes, rightBytes)
	}
	if whole != s.nodes[0].physical {
		t.Fatal("virtual total differs from scan root")
	}
}

func TestDepthPruningPreservesPhysicalUsage(t *testing.T) {
	root := t.TempDir()
	nested := filepath.Join(root, "one", "two")
	if err := os.MkdirAll(nested, 0700); err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(nested, "source")
	if err := os.WriteFile(source, make([]byte, 256*1024), 0600); err != nil {
		t.Fatal(err)
	}
	clone(t, source, filepath.Join(root, "clone"))
	full := newScanner(false, false, -1)
	if err := full.scan([]string{root}); err != nil {
		t.Fatal(err)
	}
	for _, depth := range []int{0, 1} {
		pruned := newScanner(false, false, depth)
		if err := pruned.scan([]string{root}); err != nil {
			t.Fatal(err)
		}
		if pruned.nodes[0].physical != full.nodes[0].physical || pruned.nodes[0].allocated != full.nodes[0].allocated {
			t.Fatalf("depth %d changed root usage", depth)
		}
		for _, n := range pruned.nodes[1:] {
			if n.depth > depth {
				t.Fatalf("retained node at depth %d above limit %d", n.depth, depth)
			}
		}
	}
}

func TestBitmapUnion(t *testing.T) {
	for _, sparse := range []bool{false, true} {
		bitmap := bitmapSet{sparse: sparse}
		for _, r := range []extent{{0, 4096 * 65}, {4096 * 63, 4096 * 67}, {4096 * 64, 4096 * 66}, {4096 * 80, 4096 * 82}, {4096 * 4095, 4096 * 4098}} {
			bitmap.addRange(1, r, 4096)
		}
		bitmap.addRange(2, extent{0, 4096}, 4096)
		if want := int64(73 * 4096); bitmap.physical != want {
			t.Fatalf("sparse=%v physical union = %d, want %d", sparse, bitmap.physical, want)
		}
	}
}

func TestSparseFileAndAliases(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sparse")
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := file.Truncate(1 << 30); err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteAt(make([]byte, 4096), 1<<29); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(path, filepath.Join(root, "hardlink")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(path, filepath.Join(root, "symlink")); err != nil {
		t.Fatal(err)
	}
	full := newScanner(false, false, -1)
	if err := full.scan([]string{root}); err != nil {
		t.Fatal(err)
	}
	fast := newScanner(false, false, 0)
	if err := fast.scan([]string{root}); err != nil {
		t.Fatal(err)
	}
	if full.errorCount != 0 || fast.errorCount != 0 {
		t.Fatalf("sparse scan errors: full=%+v fast=%+v", full.errors, fast.errors)
	}
	if full.nodes[0].physical != fast.nodes[0].physical || fast.nodes[0].physical == 0 {
		t.Fatalf("sparse physical mismatch: full=%d fast=%d", full.nodes[0].physical, fast.nodes[0].physical)
	}
	if fast.nodes[0].physical >= 1<<20 {
		t.Fatalf("sparse file counted holes: %d bytes", fast.nodes[0].physical)
	}
}

func TestUnreadableDirectoryIsReported(t *testing.T) {
	root := t.TempDir()
	secret := filepath.Join(root, "secret")
	if err := os.Mkdir(secret, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(secret, "hidden"), []byte("data"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(secret, 0000); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(secret, 0700) })
	s := newScanner(false, false, 0)
	if err := s.scan([]string{root}); err != nil {
		t.Fatal(err)
	}
	if s.unreadableDirectories != 1 || len(s.unreadablePaths) != 1 || s.unreadablePaths[0] != secret {
		t.Fatalf("unreadable directory not reported: count=%d paths=%v", s.unreadableDirectories, s.unreadablePaths)
	}
}

func TestDistinctTotalAcrossOperands(t *testing.T) {
	root := t.TempDir()
	left := filepath.Join(root, "left")
	right := filepath.Join(root, "right")
	if err := os.WriteFile(left, make([]byte, 256*1024), 0600); err != nil {
		t.Fatal(err)
	}
	clone(t, left, right)
	s := newScanner(false, false, 0)
	if err := s.scan([]string{left, right}); err != nil {
		t.Fatal(err)
	}
	if s.errorCount != 0 {
		t.Fatalf("unexpected errors: %+v", s.errors)
	}
	if s.nodes[0].physical != s.nodes[s.roots[0]].physical || s.nodes[0].physical != s.nodes[s.roots[1]].physical {
		t.Fatalf("total did not deduplicate clone: total=%d left=%d right=%d", s.nodes[0].physical, s.nodes[s.roots[0]].physical, s.nodes[s.roots[1]].physical)
	}
}

func TestCommonDuOptions(t *testing.T) {
	o, err := parseArgs([]string{"-sh", "-B1", "-d", "1", "--json", "/tmp"})
	if err != nil {
		t.Fatal(err)
	}
	if !o.summary || o.human || !o.json || o.blockSize != 1 || o.depth != 1 {
		t.Fatalf("unexpected options: %+v", o)
	}
	if _, err := parseArgs([]string{"--apparent-size"}); err == nil {
		t.Fatal("unsupported option was accepted")
	}
}

func TestDisplayPathFromRoot(t *testing.T) {
	if got := displayPath("/Applications", "/", "/"); got != "/Applications" {
		t.Fatalf("root display path = %q", got)
	}
}
