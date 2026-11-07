@echo off
rem Run rust-link from this folder, like "java -jar Lavalink.jar".
cd /d "%~dp0"
rust-link.exe %*
pause
