#!/usr/bin/env bash

# Test script for NNTP proxy round-robin functionality
set -e

echo "🧪 Testing NNTP Proxy Round-Robin Functionality"
echo "================================================"

# Check if the proxy is running
if ! nc -z localhost 8119 2>/dev/null; then
    echo "❌ NNTP proxy is not running on port 8119"
    echo "   Start it with: cargo run"
    exit 1
fi

echo "✅ NNTP proxy is running on port 8119"
echo ""

# Test multiple connections to see round-robin behavior
echo "🔄 Testing round-robin distribution..."
echo "   Making 6 test connections to observe server selection:"
echo ""

for i in {1..6}; do
    echo "Connection $i:"
    # Use timeout to avoid hanging if backend is not available
    echo -e "QUIT\r\n" | timeout 2 nc localhost 8119 2>/dev/null || echo "  Backend connection failed (expected for non-existent servers)"
    sleep 0.5
done

echo ""
echo "📊 Check the proxy logs to see which backend servers were selected"
echo "   Each connection should round-robin through the configured servers:"
echo "   1. news.example.com:119"
echo "   2. nntp.example.org:119" 
echo "   3. localhost:1119"
echo "   4. news.example.com:119 (back to first)"
echo "   ... and so on"

echo ""
echo "✨ Test complete! Check the proxy terminal output for routing details."
