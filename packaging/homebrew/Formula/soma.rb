class Soma < Formula
  desc "Local-first AI agent governance with verifiable, tamper-evident audit trails"
  homepage "https://github.com/radotsvetkov/soma"
  version "0.2.0"
  license "Apache-2.0"

  # These archives are the ones cargo-dist attaches to each GitHub release.
  # On a new release, bump `version` above and replace the four sha256 values
  # with the published per-archive checksums (see RELEASING.md).
  on_macos do
    on_arm do
      url "https://github.com/radotsvetkov/soma/releases/download/v0.2.0/soma-aarch64-apple-darwin.tar.xz"
      sha256 "REPLACE_WITH_SHA256_OF_soma-aarch64-apple-darwin.tar.xz"
    end
    on_intel do
      url "https://github.com/radotsvetkov/soma/releases/download/v0.2.0/soma-x86_64-apple-darwin.tar.xz"
      sha256 "REPLACE_WITH_SHA256_OF_soma-x86_64-apple-darwin.tar.xz"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/radotsvetkov/soma/releases/download/v0.2.0/soma-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "REPLACE_WITH_SHA256_OF_soma-aarch64-unknown-linux-gnu.tar.xz"
    end
    on_intel do
      url "https://github.com/radotsvetkov/soma/releases/download/v0.2.0/soma-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "REPLACE_WITH_SHA256_OF_soma-x86_64-unknown-linux-gnu.tar.xz"
    end
  end

  def install
    bin.install "soma"
  end

  test do
    assert_match "0.2.0", shell_output("#{bin}/soma --version")
  end
end
