# The Homebrew formula, for the tap `jasonmcaffee/homebrew-inillucent`.
#
#   brew install jasonmcaffee/inillucent/inillucent
#
# It installs the release tarball rather than building from source, so a `brew
# install` is a download and an unpack rather than a fifteen-minute LTO build of
# thirty-one crates. `head` builds from the repository for somebody who wants
# what is on main.
#
# To publish it: create the repository `jasonmcaffee/homebrew-inillucent` on
# GitHub, put this file in `Formula/inillucent.rb`, and fill in the two sha256
# values from the release's own SHA256SUMS. `packaging/homebrew/update.sh` does
# both of those from a built dist/ so the numbers are never typed by hand.
class Inillucent < Formula
  desc "Embedded SQL database with full-text and vector search, and an MCP server"
  homepage "https://github.com/jasonmcaffee/inillucent"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/jasonmcaffee/inillucent/releases/download/v0.1.0/inillucent-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_THE_AARCH64_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/jasonmcaffee/inillucent/releases/download/v0.1.0/inillucent-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_THE_X86_64_DARWIN_SHA256"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/jasonmcaffee/inillucent/releases/download/v0.1.0/inillucent-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_THE_X86_64_LINUX_SHA256"
    end
  end

  head do
    url "https://github.com/jasonmcaffee/inillucent.git", branch: "main"
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
    listed = pipe_output(
      "#{bin}/inillucent-mcp --db #{testpath}/probe.rdb",
      %({"jsonrpc":"2.0","id":1,"method":"tools/list"}\n)
    )
    assert_match "inillucent_query", listed
  end
end
