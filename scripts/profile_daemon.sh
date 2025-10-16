#!/bin/bash
# Helper script to profile the stentor daemon

set -e

echo "=== Stentor Daemon Profiler ==="
echo ""

# Check if daemon is running
PID=$(pgrep stentord || echo "")

if [ -z "$PID" ]; then
    echo "Error: stentord is not running"
    echo ""
    echo "Please start the daemon first:"
    echo "  ./target/release/stentord --quiet"
    echo ""
    exit 1
fi

echo "Found stentord daemon: PID $PID"
echo ""
echo "Starting perf recording..."
echo "Press Ctrl+C after you've triggered the recording and seen the Kitty windows"
echo ""

# Record with perf
sudo perf record -F 99 -p $PID --call-graph dwarf -o perf.data

echo ""
echo "Perf recording saved to perf.data"
echo ""
echo "Generating flamegraph..."

# Generate flamegraph
sudo perf script -i perf.data | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg

echo "Flamegraph saved to: flamegraph.svg"
echo ""
echo "Open it with:"
echo "  xdg-open flamegraph.svg"
