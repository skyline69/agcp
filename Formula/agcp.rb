class Agcp < Formula
  desc "Lightweight Rust proxy translating Anthropic Claude API to Google Cloud Code API"
  homepage "https://github.com/skyline69/agcp"
  version "1.0.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/skyline69/agcp/releases/download/v#{version}/agcp-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "bc82dcc396ad8a6abacae3b3a52d1e0f381e5af91f7075f8311aeccdaf3ea89b"
    else
      url "https://github.com/skyline69/agcp/releases/download/v#{version}/agcp-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "b53cddfe4fafdfc9db285a75d56240d2ddaa1073aea1d610ce957a8d0ae1a41c"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/skyline69/agcp/releases/download/v#{version}/agcp-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9f83c3553226ee713d553b3787e78bb8ebd16cd29e6186c791f55013730cf9fd"
    else
      url "https://github.com/skyline69/agcp/releases/download/v#{version}/agcp-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f6389112203a7a2b33585be669fc1552df5a1be64c00a174a1ec13b646074b08"
    end
  end

  def install
    bin.install "agcp"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/agcp --version")
  end
end
