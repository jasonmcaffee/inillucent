// The Go module lives in a subdirectory of the repository, so its import path
// carries that subdirectory and the git tag that releases it carries the same
// prefix: `packages/go/v0.1.2`, not `v0.1.2`. That is Go's own rule for a
// nested module. A separate repository would avoid the prefix; it would also be
// a second place where the source of one project lives, and the tag is cheaper.
//
// The prefix belongs to the tag and nowhere else. The version argument is the
// plain version, and `@latest` is the one to write down because it cannot go
// stale and point at a release that has been superseded:
//
//     go get github.com/Black-Rainbow-Labs/Inillucent/packages/go@latest
//
// Passing the tag name there is rejected - `go install` answers `invalid
// version: version "packages/go/v0.1.2" invalid: disallowed version string`.
//
// Do not install `packages/go/v0.1.0`. Its command directory was `cmd/inillucent`,
// so it built a program that had to overwrite itself to finish its own job.
module github.com/Black-Rainbow-Labs/Inillucent/packages/go

go 1.21
