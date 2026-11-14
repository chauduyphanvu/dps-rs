class Dps < Formula
  desc "Audio processing chain for files and live macOS system audio"
  homepage "https://github.com/chauduyphanvu/dps-rs"
  url "https://github.com/chauduyphanvu/dps-rs/releases/download/v0.1.0/dps-0.1.0-macos-universal.zip"
  # Placeholder: scripts/release.sh prints the real checksum.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
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
