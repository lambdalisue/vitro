# Rendered from packaging/homebrew/vitro.rb in lambdalisue/vitro by its release
# workflow, which fills in the version and the checksum of each archive the
# release attached. Edit it there: every release overwrites the tap's copy.
class Vitro < Formula
  desc "Disposable development VMs driven from a CLI"
  homepage "https://github.com/lambdalisue/vitro"
  version "@VERSION@"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/lambdalisue/vitro/releases/download/v@VERSION@/vitro-v@VERSION@-aarch64-apple-darwin.tar.gz"
      sha256 "@SHA256_aarch64-apple-darwin@"
    end
    on_intel do
      url "https://github.com/lambdalisue/vitro/releases/download/v@VERSION@/vitro-v@VERSION@-x86_64-apple-darwin.tar.gz"
      sha256 "@SHA256_x86_64-apple-darwin@"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/lambdalisue/vitro/releases/download/v@VERSION@/vitro-v@VERSION@-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "@SHA256_aarch64-unknown-linux-gnu@"
    end
    on_intel do
      url "https://github.com/lambdalisue/vitro/releases/download/v@VERSION@/vitro-v@VERSION@-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "@SHA256_x86_64-unknown-linux-gnu@"
    end
  end

  # Every QEMU guest needs it, and it brings the UEFI firmware an aarch64
  # guest boots from.
  depends_on "qemu"

  def install
    bin.install "vitro"
    generate_completions_from_executable(bin/"vitro", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/vitro --version")
  end
end
