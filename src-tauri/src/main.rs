// Windows 릴리즈에서 콘솔 창 숨김
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    qa_manager_desktop_lib::run()
}
