@echo off
rem Builds libtugboat_ffi.so for Android (arm64-v8a phone + x86_64 emulator) into
rem app\src\main\jniLibs and regenerates the Kotlin bindings into app\src\main\java\uniffi.
rem Run from any cmd.exe window. Needs: rust + the two android targets, cargo-ndk, the NDK.
rem Usage: build-rust.cmd            release .so (what the AAB ships)
rem        build-rust.cmd debug      debug .so (faster to build, big, slow at runtime)
setlocal
cd /d "%~dp0\..\.."

set "PROFILE=release"
set "PROFILE_FLAG=--release"
if /i "%~1"=="debug" (
  set "PROFILE=debug"
  set "PROFILE_FLAG="
)

if not defined ANDROID_NDK_HOME set "ANDROID_NDK_HOME=%LOCALAPPDATA%\Android\Sdk\ndk\30.0.16248370"
if not exist "%ANDROID_NDK_HOME%\" (
  echo NDK not found at %ANDROID_NDK_HOME%
  exit /b 1
)

set "JNI=apps\android\app\src\main\jniLibs"
set "KT=apps\android\app\src\main\java"

echo [1/4] host build (the library the bindings are read from) + uniffi-bindgen
cargo build -p tugboat-ffi --features bindgen
if errorlevel 1 exit /b 1

echo [2/4] android build via cargo-ndk (%PROFILE%)
cargo ndk -t arm64-v8a -t x86_64 --platform 31 -o "%JNI%" build %PROFILE_FLAG% -p tugboat-ffi
if errorlevel 1 exit /b 1

echo [3/4] drop stray libiroh*.so (iroh crates also declare cdylib and cargo-ndk copies them); only libtugboat_ffi.so belongs
for %%A in (arm64-v8a x86_64) do (
  for %%F in ("%JNI%\%%A\*.so") do (
    if /i not "%%~nxF"=="libtugboat_ffi.so" del "%%F"
  )
)

echo [4/4] kotlin bindings (replaces the generated package folder)
if exist "%KT%\uniffi\tugboat_ffi" rmdir /s /q "%KT%\uniffi\tugboat_ffi"
target\debug\uniffi-bindgen.exe generate target\debug\tugboat_ffi.dll --language kotlin --no-format --out-dir "%KT%"
if errorlevel 1 exit /b 1

echo OK
for %%A in (arm64-v8a x86_64) do (
  for %%F in ("%JNI%\%%A\*.so") do echo %%A\%%~nxF %%~zF bytes
)
echo %KT%\uniffi\tugboat_ffi\tugboat_ffi.kt
exit /b 0
