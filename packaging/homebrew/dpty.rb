class Dpty < Formula
  desc "Durable PTY sessions: a daemon, CLI and MCP server in one binary"
  homepage "https://jasonkneen.github.io/durable-pty/"
  url "https://github.com/jasonkneen/durable-pty/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_RELEASE_SHA256"
  license "MIT"
  head "https://github.com/jasonkneen/durable-pty.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  # `brew services start dpty` runs the daemon at login. `dpty install` does the same without Homebrew.
  service do
    run [opt_bin/"dpty", "serve"]
    keep_alive true
    log_path var/"log/dpty.log"
    error_log_path var/"log/dpty.log"
    environment_variables PATH: std_service_path_env
  end

  test do
    assert_match "dpty", shell_output("#{bin}/dpty --version")
    assert_match "not running", shell_output("DPTY_HOME=#{testpath}/home #{bin}/dpty status", 3)
  end
end
