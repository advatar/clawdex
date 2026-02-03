# App Store packaging notes (high-level)

This project is intentionally minimal. Before submitting to the Mac App Store:

- Ensure you're not downloading/installing/executing new code to add or change functionality at runtime (see App Review Guideline 2.5.2).
- Ensure the app does not auto-launch at login without user consent, and does not keep background processes running after the user quits (see Mac App Store additional requirements).
- Ensure all embedded executables are bundled and signed.

This repo includes:
- sandbox entitlements (Resources/ClawdApp.entitlements)
- helper-tool entitlements with `com.apple.security.inherit` (Resources/HelperTool.entitlements)
- a starter PrivacyInfo.xcprivacy manifest (Resources/PrivacyInfo.xcprivacy)

You must:
- Replace `YOURTEAMID` and bundle identifiers in project.yml
- Fill in PrivacyInfo.xcprivacy and App Store Connect privacy labels
