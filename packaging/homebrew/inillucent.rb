# The Homebrew formula, for the tap `Black-Rainbow-Labs/homebrew-inillucent`.
#
#   brew install black-rainbow-labs/inillucent/inillucent
#
# It installs the release tarball rather than building from source, so a `brew
# install` is a download and an unpack rather than a fifteen-minute LTO build of
# thirty-one crates. `head` builds from the repository for somebody who wants
# what is on main.
#
# The URLs point at inillucent.com rather than at a GitHub release, because the
# repository is private and a private repository's release assets are private
# too: `brew install` would get a 404. The tap itself is a separate, public
# repository holding only this file, so the source can stay private while the
# formula is fetchable.
#
# The `head` block below is the exception, and it does not work today. It clones
# the repository, which is private, so `brew install --HEAD` gets the same 404
# the release URLs were moved off GitHub to avoid. `brew install` is unaffected.
# It starts working the day the repository is public; `PUBLISHING.md` has that
# decision, and `node tools/check-public-urls.mjs` reports this URL among the
# others that a signed-out reader cannot open.
#
# To publish it: create the repository `Black-Rainbow-Labs/homebrew-inillucent` on
# GitHub, put this file in `Formula/inillucent.rb`, and fill in the sha256
# values from the release's own SHA256SUMS. `packaging/homebrew/update.sh` does
# both of those from a built dist/ so the numbers are never typed by hand.
class Inillucent < Formula
  desc "Embedded SQL database with full-text and vector search, and an MCP server"
  homepage "https://inillucent.com"
  version "1.0.31"
  license "MIT"

  # One universal archive covers both Apple architectures, so macOS needs no
  # on_arm / on_intel split: the same file is correct either way.
  on_macos do
    url "https://inillucent.com/downloads/inillucent-0.1.2-universal-apple-darwin.tar.gz"
    sha256 "REPLACE_WITH_THE_UNIVERSAL_DARWIN_SHA256"
  end

  on_linux do
    on_intel do
      url "https://inillucent.com/downloads/inillucent-0.1.2-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_THE_X86_64_LINUX_SHA256"
    end
    on_arm do
      url "https://inillucent.com/downloads/inillucent-0.1.2-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_THE_AARCH64_LINUX_SHA256"
    end
  end

  head do
    url "https://github.com/Black-Rainbow-Labs/Inillucent.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", "--locked", "--path", "crates/inillucent-cli", "--root", prefix
      system "cargo", "build", "--release", "--locked", "-p", "inillucent-migrate", "-p", "inillucent-driver-capi"
      bin.install "target/release/inillucent-migrate"
      lib.install Dir["target/release/libinillucent_driver_capi.*"]
      include.install "drivers/inillucent-driver-capi/include/inillucent_driver.h"
    else
      bin.install Dir["bin/*"]
      lib.install Dir["lib/*"]
      include.install "include/inillucent_driver.h"
    end
    doc.install "README.md"
    doc.install "DRIVER.md" if File.exist?("DRIVER.md")
  end

  def caveats
    <<~CAVEATS
      To give an AI agent inillucent's commands, add this to its MCP configuration:

        "inillucent": {
          "type": "local",
          "command": ["#{opt_bin}/inillucent-mcp", "--db", "app.rdb"],
          "enabled": true
        }
    CAVEATS
  end

  # The test is what `brew test` runs and what the tap's CI runs on every bump.
  # It is deliberately an end-to-end one: a formula whose test only checks
  # `--version` will happily ship a binary that cannot open a file, which is the
  # failure a packaging mistake actually produces.
  test do
    system bin/"inillucent", "create", testpath/"probe.rdb"
    system bin/"inillucent", "--db", testpath/"probe.rdb", "exec",
           "CREATE TABLE t (a INTEGER, b TEXT)"
    system bin/"inillucent", "--db", testpath/"probe.rdb", "exec",
           "INSERT INTO t VALUES (1, 'one')"
    rows = shell_output("#{bin}/inillucent --db #{testpath}/probe.rdb query \"SELECT b FROM t\"")
    assert_match "one", rows

    # And the MCP server answers a real request on standard input, because that
    # is the half of this package a `--version` check cannot reach at all.
    # The whole lifecycle: the server refuses tools/list until initialize and
    # notifications/initialized have both arrived, and refuses an initialize
    # that carries no protocolVersion.
    handshake = [
      %({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18",) +
        %("capabilities":{},"clientInfo":{"name":"brew-test","version":"1"}}}),
      %({"jsonrpc":"2.0","method":"notifications/initialized"}),
      %({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
    ].join("\n")
    listed = pipe_output("#{bin}/inillucent-mcp --db #{testpath}/probe.rdb", "#{handshake}\n")
    assert_match "inillucent_query", listed
  end
end
