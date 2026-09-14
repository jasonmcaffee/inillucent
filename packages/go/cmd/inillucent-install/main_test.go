// The installer's platform table, checked against the archives the release
// actually publishes.
//
// Invariant: every platform this installer claims to support names a file
// `packaging/macos/release-macos.sh` or `packaging/release-all.ps1` writes. A
// wrapper that asks for a file nobody publishes fails at the download with a
// 404, which reads to whoever ran it as "the project has no build for my
// machine" rather than as a bug in the installer.
//
// The defect this was written for (task-1932, H12): macOS resolved to
// `aarch64-apple-darwin` and `x86_64-apple-darwin`, and the release publishes
// neither. It builds both Apple targets and `lipo`s them into one
// `universal-apple-darwin` archive, which is what `packaging/install.sh` has
// always asked for. Every Mac running `go install ...inillucent-install` was
// told there was no build for it.
package main

import "testing"

// Every platform resolves to a triple the release publishes.
//
// The list on the right is the set of archive names in the release, and it is
// written out rather than derived so that a change to the release's naming
// fails here rather than in somebody's terminal.
func TestTargetNamesAPublishedArchive(t *testing.T) {
	published := map[string]bool{
		"x86_64-pc-windows-msvc":    true,
		"universal-apple-darwin":    true,
		"x86_64-unknown-linux-gnu":  true,
		"aarch64-unknown-linux-gnu": true,
	}
	cases := []struct {
		goos   string
		goarch string
		want   string
	}{
		{"windows", "amd64", "x86_64-pc-windows-msvc"},
		{"darwin", "arm64", "universal-apple-darwin"},
		{"darwin", "amd64", "universal-apple-darwin"},
		{"linux", "amd64", "x86_64-unknown-linux-gnu"},
		{"linux", "arm64", "aarch64-unknown-linux-gnu"},
	}
	for _, one := range cases {
		got, err := targetFor(one.goos, one.goarch)
		if err != nil {
			t.Fatalf("%s/%s: %v", one.goos, one.goarch, err)
		}
		if got != one.want {
			t.Errorf("%s/%s resolved to %q, want %q", one.goos, one.goarch, got, one.want)
		}
		if !published[got] {
			t.Errorf("%s/%s asks for %q, which the release does not publish",
				one.goos, one.goarch, got)
		}
	}
}

// A macOS install asks for the file release-macos.sh writes.
//
// The name is built in one place and asserted whole, because the defect was in
// the middle of it: the version and the extension were right and the triple was
// a file that has never existed.
func TestMacOSArchiveIsTheOneTheReleaseWrites(t *testing.T) {
	triple, err := targetFor("darwin", "arm64")
	if err != nil {
		t.Fatalf("darwin/arm64: %v", err)
	}
	got := archiveName("0.1.1", triple, "darwin")
	// `packaging/macos/release-macos.sh` line 88:
	//     name="inillucent-$version-universal-apple-darwin"
	// and line 144 appends `.tar.gz`.
	want := "inillucent-0.1.1-universal-apple-darwin.tar.gz"
	if got != want {
		t.Errorf("darwin/arm64 downloads %q, and release-macos.sh writes %q", got, want)
	}
}

// Windows gets the zip and everything else the tarball.
func TestArchiveExtensionFollowsThePlatform(t *testing.T) {
	if got := archiveName("0.1.1", "x86_64-pc-windows-msvc", "windows"); got !=
		"inillucent-0.1.1-x86_64-pc-windows-msvc.zip" {
		t.Errorf("windows downloads %q", got)
	}
	if got := archiveName("0.1.1", "x86_64-unknown-linux-gnu", "linux"); got !=
		"inillucent-0.1.1-x86_64-unknown-linux-gnu.tar.gz" {
		t.Errorf("linux downloads %q", got)
	}
}

// A platform with no release is refused by name rather than downloaded.
func TestAnUnbuiltPlatformIsRefused(t *testing.T) {
	if _, err := targetFor("freebsd", "amd64"); err == nil {
		t.Error("freebsd/amd64 resolved to a triple; there is no release for it")
	}
}

// The version this wrapper installs is pinned, and `latest` is the opt in.
//
// **`go install ...@v0.1.1` installing 0.1.2 is the thing this prevents**
// (task-1932, H12). With no version the installer used to fetch
// `downloads/VERSION`, so the same command on two days installed two different
// programs and the Go module version said nothing about what it would put on
// the machine.
func TestTheNativeVersionIsPinned(t *testing.T) {
	if nativeVersion == "" || nativeVersion == "latest" {
		t.Fatalf("nativeVersion is %q, so the installer has no version of its own", nativeVersion)
	}
	// It has to look like a release, because it is pasted into a URL.
	for _, character := range nativeVersion {
		if (character < '0' || character > '9') && character != '.' {
			t.Fatalf("nativeVersion %q is not a plain version", nativeVersion)
		}
	}
}
