// Texts of the update check, in one place so a test can pin them. The check itself runs in Rust
// (src-tauri/src/updates.rs); the webview only shows the switch and the banner.

export const UPDATE_CHECK_LABEL = "Check for updates";
export const UPDATE_CHECK_HELP = "Once a day, from GitHub. Sends nothing about you or your files.";

/** The banner line while a verified update waits. */
export const updateBannerText = (version: string): string => `UPDATE READY - Tugboat ${version}`;

/** The banner line from the click on Install until the installer starts (or Windows says no). */
export const UPDATE_INSTALLING_TEXT = "INSTALLING - Windows will ask for permission";
