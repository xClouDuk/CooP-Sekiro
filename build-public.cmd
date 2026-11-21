@echo off
rem Public release build (NexusMods, GitHub Releases): coop-sekiro-v<version>.zip
rem Drag-and-drop layout: coop_sekiro_launcher.exe + CoopSekiro\ (no me3 needed).
setlocal
set "BUILD_DIR=%~dp0"
if "%BUILD_DIR:~-1%"=="\" set "BUILD_DIR=%BUILD_DIR:~0,-1%"

rem Removed the hardcoded CooP-Sekiro folder name requirement
set "ROOT=%BUILD_DIR%"

pushd "%ROOT%" || (pause & exit /b 1)
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
set "VCVARS="
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not defined VCVARS if exist "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if not defined VCVARS if exist "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not defined VCVARS if exist "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat"
if not defined VCVARS if exist "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not defined VCVARS if exist "%VSWHERE%" for /f "usebackq delims=" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSROOT=%%I"
if not defined VCVARS if defined VSROOT if exist "%VSROOT%\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=%VSROOT%\VC\Auxiliary\Build\vcvars64.bat"
if not defined VCVARS (echo Visual Studio C++ vcvars64.bat not found & pause & exit /b 1)
call "%VCVARS%" >nul || (pause & exit /b 1)
set "CARGO_TARGET_DIR=%BUILD_DIR%\target-public"

cargo build --manifest-path "%ROOT%\Cargo.toml" --release -p sekiro_coop --no-default-features --features dev-native --config "build.rustflags=['--remap-path-prefix=%USERPROFILE%=~', '--remap-path-prefix=%ROOT%=.', '-Clink-arg=/PDBALTPATH:sekiro_coop.pdb']" || (pause & exit /b 1)

set "LAUNCHER_EXE="
if "%REBUILD_LAUNCHER%"=="1" goto :build_launcher
if exist "%ROOT%\prebuilt\coop_sekiro_launcher.exe" set "LAUNCHER_EXE=%ROOT%\prebuilt\coop_sekiro_launcher.exe"
if not defined LAUNCHER_EXE if exist "%ROOT%\..\Build and Test\prebuilt\coop_sekiro_launcher.exe" set "LAUNCHER_EXE=%ROOT%\..\Build and Test\prebuilt\coop_sekiro_launcher.exe"
:build_launcher
if defined LAUNCHER_EXE (
  echo Using prebuilt verified coop_sekiro_launcher.exe
) else (
  cargo build --manifest-path "%ROOT%\Cargo.toml" --release -p coop-launcher --config "build.rustflags=['--remap-path-prefix=%USERPROFILE%=~', '--remap-path-prefix=%ROOT%=.', '-Clink-arg=/PDBALTPATH:coop_sekiro_launcher.pdb']" || (pause & exit /b 1)
  set "LAUNCHER_EXE=%CARGO_TARGET_DIR%\release\coop_sekiro_launcher.exe"
)

for /f "tokens=2 delims==;" %%V in ('findstr /b /c:"pub const MOD_VERSION" "%ROOT%\crates\coop-dll\src\ui.rs"') do for %%A in (%%V) do set "VER=%%~A"
if "%VER%"=="" (echo MOD_VERSION not found & pause & exit /b 1)

set "S=%BUILD_DIR%\coop-sekiro"
if exist "%S%" rmdir /s /q "%S%"
mkdir "%S%\CoopSekiro\licenses" || (pause & exit /b 1)
copy /y "%LAUNCHER_EXE%" "%S%\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\HOW-TO-PLAY.txt" "%S%\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\UDP-GUIDE.txt" "%S%\" >nul || (pause & exit /b 1)
copy /y "%CARGO_TARGET_DIR%\release\sekiro_coop.dll" "%S%\CoopSekiro\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\sekiro_coop_settings.ini" "%S%\CoopSekiro\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\HOW-TO-PLAY.txt" "%S%\CoopSekiro\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\UDP-GUIDE.txt" "%S%\CoopSekiro\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\LICENSE.txt" "%S%\CoopSekiro\" >nul || (pause & exit /b 1)
copy /y "%ROOT%\ship-public\licenses\*" "%S%\CoopSekiro\licenses\" >nul || (pause & exit /b 1)

set "Z=%BUILD_DIR%\coop-sekiro-v%VER%.zip"
if exist "%Z%" del "%Z%"
pushd "%S%" || (pause & exit /b 1)
"%SystemRoot%\System32\tar.exe" -a -c -f "%Z%" coop_sekiro_launcher.exe HOW-TO-PLAY.txt UDP-GUIDE.txt CoopSekiro || (popd & pause & exit /b 1)
popd
popd
echo Ready: %Z%
pause