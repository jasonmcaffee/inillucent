// Command inillucent installs the inillucent binaries with `go install`.
//
//	go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest
//
// inillucent is written in Rust, so `go install` cannot build it - what it can
// do is build this, which downloads the release for the machine it is running
// on, checks its SHA-256 against the release's published SHA256SUMS, and puts
// the four programs next to itself in GOBIN. After that `inillucent` is a
// command, and this program has done its job.
//
// It is deliberately the whole install: no shell script fetched and piped, no
// package manager, and the checksum is verified rather than trusted. A Go
// developer who has `go install` has everything.
//
//	inillucent-install                 # the latest release
//	inillucent-install -version 0.1.0  # a specific one
//	inillucent-install -dir ./bin      # somewhere other than GOBIN
package main

import (
	"archive/tar"
	"archive/zip"
	"bufio"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

const repository = "Black-Rainbow-Labs/Inillucent"

// Where the archives live. The site rather than the GitHub release, because it
// is the one URL every installer on every platform reads: install.ps1,
// install.sh, the Homebrew formula and the PHP bin/inillucent-install all read
// from here and check the SHA-256 against the published SHA256SUMS. It began as
// a necessity - a private repository's release assets are private too, and an
// unauthenticated request for one answered 404 - and stayed a choice after
// task-1961 made the repository public, because one place to publish is one
// place to get wrong.
const downloads = "https://inillucent.com/downloads"

// The release this wrapper was built and tested against.
//
// **Pinned, and newest is a flag (task-1932, H12).** With no version given this
// used to fetch `downloads/VERSION` and install whatever was newest, which
// makes `go install ...@v0.1.1` install something other than 0.1.1 and makes
// two machines running the same command on different days get different
// programs. A wrapper that cannot say which native version it goes with cannot
// be depended on by anything.
//
// `-version latest` still asks the server, for somebody who wants the newest on
// purpose.
const nativeVersion = "1.0.29"

// programs are the four the release ships.
var programs = []string{"inillucent", "inillucent-shell", "inillucent-mcp", "inillucent-migrate"}

// target returns the Rust target triple this machine's releases are built for.
func target() (string, error) {
	return targetFor(runtime.GOOS, runtime.GOARCH)
}

// targetFor returns the Rust target triple a platform's releases are built for,
// and says so plainly when there is not one rather than downloading something
// that will not run.
//
// It takes the platform rather than reading it so that every row can be checked
// from one machine. The row that was wrong - macOS - could not be checked from
// the machine the release is cut on, which is how it stayed wrong.
func targetFor(goos string, goarch string) (string, error) {
	switch goos + "/" + goarch {
	case "windows/amd64":
		return "x86_64-pc-windows-msvc", nil
	// **One universal archive covers both Apple architectures.** The release
	// builds the two Apple targets and `lipo`s them together; nothing named
	// `aarch64-apple-darwin` or `x86_64-apple-darwin` is ever published, so
	// asking for either was asking for a file that has never existed and the
	// installer answered "no build for your platform" on every Mac
	// (task-1932, H12). `packaging/install.sh` has always had this right.
	case "darwin/arm64":
		return "universal-apple-darwin", nil
	case "darwin/amd64":
		return "universal-apple-darwin", nil
	case "linux/amd64":
		return "x86_64-unknown-linux-gnu", nil
	case "linux/arm64":
		return "aarch64-unknown-linux-gnu", nil
	default:
		return "", fmt.Errorf(
			"there is no inillucent release for %s/%s yet.\n"+
				"  Build it from source instead:  cargo install inillucent-cli\n"+
				"  Or open an issue: https://github.com/%s/issues",
			goos, goarch, repository)
	}
}

// archiveName returns the file a release publishes for one version and target.
//
// The one place the name is built, so a test can assert it against the name
// `packaging/macos/release-macos.sh` writes without running an install. goos
// decides the container format and nothing else.
func archiveName(version string, triple string, goos string) string {
	extension := ".tar.gz"
	if goos == "windows" {
		extension = ".zip"
	}
	return fmt.Sprintf("inillucent-%s-%s%s", version, triple, extension)
}

// latest asks GitHub which release is newest.
func latest() (string, error) {
	// VERSION, beside the archives, is what says which release is current. One
	// file on the same host as everything else, so there is no second service to
	// be reachable and no API that can answer differently.
	body, err := fetch(downloads + "/VERSION")
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(string(body)), "v")), nil
}

// fetch downloads a URL into memory, refusing anything that is not a 200.
func fetch(url string) ([]byte, error) {
	client := &http.Client{Timeout: 10 * time.Minute}
	response, err := client.Get(url)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("%s answered %s", url, response.Status)
	}
	return io.ReadAll(response.Body)
}

// verify refuses an archive whose hash is not the one the release published.
//
// The whole reason SHA256SUMS exists. A downloader that skips this has turned a
// truncated or tampered transfer into an installed program.
func verify(archive []byte, sums []byte, name string) error {
	digest := sha256.Sum256(archive)
	actual := hex.EncodeToString(digest[:])
	scanner := bufio.NewScanner(strings.NewReader(string(sums)))
	for scanner.Scan() {
		fields := strings.Fields(scanner.Text())
		if len(fields) >= 2 && fields[len(fields)-1] == name {
			if fields[0] != actual {
				return fmt.Errorf(
					"%s does not match its published checksum.\n  expected %s\n  got      %s",
					name, fields[0], actual)
			}
			fmt.Printf("checksum ok (%s)\n", actual)
			return nil
		}
	}
	return fmt.Errorf("SHA256SUMS does not list %s", name)
}

// listed reports whether SHA256SUMS names one archive.
//
// @param sums - the SHA256SUMS body
// @param name - the archive file name
func listed(sums []byte, name string) bool {
	for _, published := range names(sums) {
		if published == name {
			return true
		}
	}
	return false
}

// names returns every file SHA256SUMS lists, in the order it lists them.
//
// @param sums - the SHA256SUMS body
func names(sums []byte) []string {
	var out []string
	scanner := bufio.NewScanner(strings.NewReader(string(sums)))
	for scanner.Scan() {
		fields := strings.Fields(scanner.Text())
		if len(fields) >= 2 {
			out = append(out, fields[len(fields)-1])
		}
	}
	return out
}

// install writes one program out of the archive into the destination.
func install(into, name string, contents []byte) error {
	path := filepath.Join(into, name)
	if err := os.WriteFile(path, contents, 0o755); err != nil {
		return err
	}
	fmt.Printf("  %s\n", path)
	return nil
}

// wanted reports whether a path inside the archive is one of the four programs,
// and returns the name to write it as.
func wanted(inside string) (string, bool) {
	name := filepath.Base(filepath.ToSlash(inside))
	for _, program := range programs {
		if name == program || name == program+".exe" {
			// Only from bin/, so a file that merely shares a name with a
			// program - a README, a symbol file - is not installed over it.
			if strings.Contains(filepath.ToSlash(inside), "/bin/") {
				return name, true
			}
		}
	}
	return "", false
}

// unpackZip installs the programs out of a Windows release archive.
func unpackZip(archive []byte, into string) (int, error) {
	reader, err := zip.NewReader(strings.NewReader(string(archive)), int64(len(archive)))
	if err != nil {
		return 0, err
	}
	found := 0
	for _, entry := range reader.File {
		name, ok := wanted(entry.Name)
		if !ok {
			continue
		}
		opened, err := entry.Open()
		if err != nil {
			return found, err
		}
		contents, err := io.ReadAll(opened)
		opened.Close()
		if err != nil {
			return found, err
		}
		if err := install(into, name, contents); err != nil {
			return found, err
		}
		found++
	}
	return found, nil
}

// unpackTar installs the programs out of a macOS or Linux release archive.
func unpackTar(archive []byte, into string) (int, error) {
	unzipped, err := gzip.NewReader(strings.NewReader(string(archive)))
	if err != nil {
		return 0, err
	}
	defer unzipped.Close()
	reader := tar.NewReader(unzipped)
	found := 0
	for {
		header, err := reader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return found, err
		}
		name, ok := wanted(header.Name)
		if !ok {
			continue
		}
		contents, err := io.ReadAll(reader)
		if err != nil {
			return found, err
		}
		if err := install(into, name, contents); err != nil {
			return found, err
		}
		found++
	}
	return found, nil
}

// destination returns where to put the programs: GOBIN, then GOPATH/bin, then
// the directory this program is running from - which is where `go install` put
// it, and so the place already on the caller's PATH.
func destination() string {
	if named := os.Getenv("GOBIN"); named != "" {
		return named
	}
	if named := os.Getenv("GOPATH"); named != "" {
		return filepath.Join(named, "bin")
	}
	if home, err := os.UserHomeDir(); err == nil {
		return filepath.Join(home, "go", "bin")
	}
	if self, err := os.Executable(); err == nil {
		return filepath.Dir(self)
	}
	return "."
}

func main() {
	version := flag.String(
		"version",
		nativeVersion,
		"the release to install; `latest` asks the server for the newest",
	)
	into := flag.String("dir", "", "where to put the programs; GOBIN by default")
	flag.Parse()

	triple, err := target()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	if *version == "latest" || *version == "" {
		found, err := latest()
		if err != nil {
			fmt.Fprintf(os.Stderr, "could not work out the latest release: %v\n", err)
			fmt.Fprintln(os.Stderr, "Pass -version to name one.")
			os.Exit(1)
		}
		*version = found
	}
	if *into == "" {
		*into = destination()
	}
	if err := os.MkdirAll(*into, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "could not make %s: %v\n", *into, err)
		os.Exit(1)
	}

	name := archiveName(*version, triple, runtime.GOOS)

	// SHA256SUMS lists every archive the release published, and it is needed to
	// verify the download anyway - so it is fetched first and used to answer
	// whether this machine has a build at all. Asking for the archive first
	// makes an unpublished platform a bare 404 instead of a sentence.
	sums, err := fetch(downloads + "/SHA256SUMS")
	if err != nil {
		fmt.Fprintf(os.Stderr, "could not fetch SHA256SUMS: %v\n", err)
		os.Exit(1)
	}
	if !listed(sums, name) {
		fmt.Fprintf(os.Stderr, "inillucent %s has no build for %s yet.\n", *version, triple)
		fmt.Fprintln(os.Stderr, "  published in this release:")
		for _, published := range names(sums) {
			fmt.Fprintf(os.Stderr, "    %s\n", published)
		}
		fmt.Fprintln(os.Stderr, "  Build it from source instead:")
		fmt.Fprintf(os.Stderr, "    git clone https://github.com/%s\n", repository)
		fmt.Fprintln(os.Stderr, "    cargo build --release -p inillucent-cli")
		os.Exit(1)
	}

	fmt.Printf("downloading inillucent %s for %s\n", *version, triple)
	archive, err := fetch(downloads + "/" + name)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		os.Exit(1)
	}
	if err := verify(archive, sums, name); err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		os.Exit(1)
	}

	fmt.Printf("installing into %s\n", *into)
	var found int
	if runtime.GOOS == "windows" {
		found, err = unpackZip(archive, *into)
	} else {
		found, err = unpackTar(archive, *into)
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		os.Exit(1)
	}
	if found != len(programs) {
		fmt.Fprintf(os.Stderr,
			"the archive held %d of the %d programs. This is a packaging fault, not your mistake:\n"+
				"  please report it at https://github.com/%s/issues\n", found, len(programs), repository)
		os.Exit(1)
	}

	fmt.Printf("\ninillucent %s is installed.\n\n", *version)
	fmt.Println("Try:")
	fmt.Println("  inillucent create app.rdb")
	fmt.Println(`  inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"`)
	fmt.Println(`  inillucent --db app.rdb query "SELECT * FROM notes"`)
	fmt.Println("  inillucent help")
}
