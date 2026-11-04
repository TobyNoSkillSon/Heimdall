# Prepared, not published. Copy into tobynoskillson/tap only when a release is published.
cask "heimdall-alpha" do
  version "0.1.0-alpha.1"
  # Placeholder: scripts/package_release.sh prints the zip's sha256 ("Cask lines: ...") for each release.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/TobyNoSkillSon/Heimdall/releases/download/v#{version}/Heimdall-Alpha-#{version}-arm64.zip"
  name "Heimdall Alpha"
  desc "Voice assistant for macOS"
  homepage "https://github.com/TobyNoSkillSon/Heimdall"

  # Alpha releases are GitHub prereleases, which both GitHub livecheck strategies skip; the app updates itself.
  livecheck do
    skip "Updates itself; alpha releases are GitHub prereleases"
  end

  auto_updates true
  depends_on arch: :arm64
  # Info.plist's LSMinimumSystemVersion is 14.4; macOS itself refuses to open the app on 14.0 to 14.3.
  depends_on macos: :sonoma

  app "Heimdall Alpha.app"

  # Heimdall Alpha is signed with its own certificate, not notarised: clear the download flag so Gatekeeper does not
  # block the first launch.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/Heimdall Alpha.app"]
  end

  uninstall quit:   "com.tobynoskillson.heimdall",
            # The in-app updater's folder beside the app: the one rollback copy of the previous version.
            delete: "#{appdir}/.heimdall-update"

  # The user's chosen home folder and its .heimdall folder (settings, memories, archive, skills) are never zapped:
  # they are the user's data, may sit anywhere the user picked, and outlive any one install.
  zap trash: [
    "~/Library/Application Support/com.tobynoskillson.heimdall",
    "~/Library/Caches/com.tobynoskillson.heimdall",
    "~/Library/HTTPStorages/com.tobynoskillson.heimdall",
    "~/Library/Preferences/com.tobynoskillson.heimdall.plist",
  ]
end
