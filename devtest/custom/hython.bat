@echo off
rem custom hython wrapper for devtest (no numeric version in path on purpose)
set "PYTHONPATH=%~dp0.."
python %*
