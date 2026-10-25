@echo off
setlocal
cd /d "%~dp0"
set "PATH=%~dp0;%SystemRoot%\System32;%SystemRoot%"
set "XDG_DATA_DIRS=%~dp0share"
set "GSETTINGS_SCHEMA_DIR=%~dp0share\glib-2.0\schemas"
set "GDK_PIXBUF_MODULEDIR=%~dp0lib\gdk-pixbuf-2.0\2.10.0\loaders"
set "GTK_A11Y=none"
"%~dp0lan-mouse.exe" --config "%~dp0config.toml" %*
if errorlevel 1 pause
