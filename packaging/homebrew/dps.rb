# The release workflow of chauduyphanvu/dps-rs writes this file to
# chauduyphanvu/homebrew-dps as Formula/dps.rb, filling in the version and
# the checksum of the release's zip.
class Dps < Formula
  desc "Audio processing chain for files and live macOS system audio"
  homepage "https://github.com/chauduyphanvu/dps-rs"
  url "https://github.com/chauduyphanvu/dps-rs/releases/download/v@VERSION@/dps-@VERSION@-macos-universal.zip"
  sha256 "@SHA256@"
  license "MIT"

  depends_on macos: :sonoma

  def install
    bin.install "dps"
  end

  def caveats
    <<~EOS
      Live mode needs macOS 14.2 or later and the "Screen & System Audio
      Recording" permission: grant it to your terminal app for `dps`, and to
      `dps` itself for `dps start`. Run `dps doctor` to check the setup.

      Files other than WAV need ffmpeg:
        brew install ffmpeg
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dps --version")
    assert_match "music", shell_output("#{bin}/dps presets")
  end
end
