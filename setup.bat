@echo off
echo Setting up Radial Menu Builder...

:: Check if Node.js is installed
node --version >nul 2>&1
if errorlevel 1 (
    echo Error: Node.js is not installed. Please install from https://nodejs.org/
    pause
    exit /b 1
)

:: Check if Rust is installed
rustc --version >nul 2>&1
if errorlevel 1 (
    echo Error: Rust is not installed. Please install from https://rustup.rs/
    pause
    exit /b 1
)

echo Installing Node.js dependencies...
npm install

echo Building Rust dependencies...
cd src-tauri
cargo build
cd ..

echo Setup complete! You can now run:
echo   npm run dev    (for development)
echo   npm run build  (for production build)
pause