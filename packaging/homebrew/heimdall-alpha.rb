cask "heimdall-alpha" do
  version "0.1.0-alpha.2"
  sha256 "d0a0d9020545afa791c22c691ab5e33c1af1c96ea53d9b4ce72fad313e6ae425"

  url "https://github.com/TobyNoSkillSon/Heimdall/releases/download/v#{version}/Heimdall-Alpha-#{version}-arm64.zip"
  name "Heimdall Alpha"
  desc "Voice assistant that sees and hears your screen"
  homepage "https://github.com/TobyNoSkillSon/Heimdall"

  # Alpha releases are GitHub prereleases, which both GitHub livecheck strategies skip; a new alpha bumps this cask.
  livecheck do
    skip "Alpha releases are GitHub prereleases"
  end

  depends_on arch: :arm64
  # Info.plist's LSMinimumSystemVersion is 14.4; macOS itself refuses to open the app on 14.0 to 14.3.
  depends_on macos: :sonoma

  app "Heimdall Alpha.app"

  # Heimdall Alpha is ad-hoc signed, not notarised: clear the download flag so Gatekeeper does not block the first
  # launch.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/Heimdall Alpha.app"]
  end

  uninstall quit: "com.tobynoskillson.heimdall"

  # The user's chosen home folder and its .heimdall folder (settings, memories, archive, skills) are never zapped:
  # they are the user's data, may sit anywhere the user picked, and outlive any one install.
  zap trash: [
    "~/Library/Application Support/com.tobynoskillson.heimdall",
    "~/Library/Caches/com.tobynoskillson.heimdall",
    "~/Library/HTTPStorages/com.tobynoskillson.heimdall",
    "~/Library/Preferences/com.tobynoskillson.heimdall.plist",
  ]
end
