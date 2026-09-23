package main

import (
	"os"
	"strconv"
	"testing"
)

func BenchmarkScanSummary(b *testing.B) {
	path := os.Getenv("DU_APFS_BENCH_PATH")
	if path == "" {
		b.Skip("set DU_APFS_BENCH_PATH")
	}
	for i := 0; i < b.N; i++ {
		s := newScanner(false, false, 0)
		if value := os.Getenv("DU_APFS_BENCH_WORKERS"); value != "" {
			workers, err := strconv.Atoi(value)
			if err != nil || workers < 1 {
				b.Fatal("invalid DU_APFS_BENCH_WORKERS")
			}
			s.workers = workers
		}
		if err := s.scan([]string{path}); err != nil {
			b.Fatal(err)
		}
	}
}
