# AppImage update control files

The bdinfo-rs GUI AppImages embed the raw URL of the `.zsync` files on this branch as their
update information, in the form
`https://raw.githubusercontent.com/agentjp/bdinfo-rs/appimage-update/<name>.AppImage.zsync`.
Each file describes the newest released AppImage of its architecture. CI writes them at every
GUI release; do not edit, rename or delete this branch.
