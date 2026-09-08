// The Go module lives in a subdirectory of the repository, so its import path
// carries that subdirectory and its tags are prefixed with it:
//
//     go get github.com/jasonmcaffee/inillucent/packages/go@packages/go/v0.1.0
//
// That is Go's own rule for a nested module and it is why the version tags for
// this package are not the plain `v0.1.0` the release uses. A separate
// repository would avoid the prefix; a separate repository would also be a
// second place where the source of one project lives, and the tag is cheaper.
module github.com/jasonmcaffee/inillucent/packages/go

go 1.21
