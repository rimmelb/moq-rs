#!/bin/bash

# Cross-Layer Congestion Control Demo Runner
# This script helps you run and monitor the cross-layer system

echo "🚀 Cross-Layer Congestion Control Demo Runner"
echo "=============================================="
echo ""
echo "This demo will show you:"
echo "  🚦 Smart CWND reduction decisions"
echo "  📊 Real-time cross-layer metrics"
echo "  🎯 Deadline-aware scheduling (future)"
echo "  📡 Enhanced bandwidth monitoring"
echo ""

show_usage() {
    echo "Usage: $0 [relay|pub|sub|all|demo]"
    echo ""
    echo "Commands:"
    echo "  relay    - Start relay with cross-layer monitoring"
    echo "  pub      - Start publisher with cross-layer monitoring"
    echo "  sub      - Start subscriber with cross-layer monitoring"
    echo "  all      - Start all components in separate terminals (requires tmux)"
    echo "  demo     - Run cross-layer demo example"
    echo "  monitor  - Show real-time bandwidth monitoring"
    echo ""
    echo "Environment variables:"
    echo "  PORT=4443              - Relay port (default: 4443)"
    echo "  RUST_LOG=debug         - Log level (default: debug)"
    echo "  DETAILED_LOGS=true     - Enable detailed logging"
    echo ""
}

run_relay() {
    echo "🏃 Starting Relay with Cross-Layer Monitoring..."
    echo "Look for these log patterns:"
    echo "  🚦 Smart CC: CWND reduction recommended"
    echo "  📊 Cross-layer: RTT=XYms, CWND=XYZ, Loss=X.XX%"
    echo "  📡 [BANDWIDTH DETAIL] Recv: X.X Mbps | Send: X.X Mbps"
    echo ""
    ./dev/relay_cross_layer
}

run_pub() {
    echo "📡 Starting Publisher with Cross-Layer Monitoring..."
    echo "The publisher will show bandwidth usage and cross-layer metrics"
    echo ""
    ./dev/pub_cross_layer
}

run_sub() {
    echo "📺 Starting Subscriber with Cross-Layer Monitoring..."
    echo "The subscriber will show received bandwidth and metrics"
    echo ""
    ./dev/sub_cross_layer
}

run_all() {
    if ! command -v tmux &> /dev/null; then
        echo "❌ tmux is required for 'all' mode. Install with:"
        echo "   sudo apt install tmux"
        echo ""
        echo "Or run components manually in separate terminals:"
        echo "   Terminal 1: ./dev/run_cross_layer_demo.sh relay"
        echo "   Terminal 2: ./dev/run_cross_layer_demo.sh pub"
        echo "   Terminal 3: ./dev/run_cross_layer_demo.sh sub"
        exit 1
    fi

    echo "🖥️  Starting all components in tmux session..."

    # Create new tmux session
    tmux new-session -d -s cross_layer_demo

    # Split into 3 panes
    tmux split-window -h
    tmux split-window -v
    tmux select-pane -t 0
    tmux split-window -v

    # Run components in each pane
    tmux send-keys -t 0 "cd '/mnt/c/Users/Rimmel Botond/Documents/bme/hatodikfelev/onlab/moq-rs' && ./dev/relay_cross_layer" Enter
    tmux send-keys -t 1 "cd '/mnt/c/Users/Rimmel Botond/Documents/bme/hatodikfelev/onlab/moq-rs' && sleep 3 && ./dev/pub_cross_layer" Enter
    tmux send-keys -t 2 "cd '/mnt/c/Users/Rimmel Botond/Documents/bme/hatodikfelev/onlab/moq-rs' && sleep 5 && ./dev/sub_cross_layer" Enter
    tmux send-keys -t 3 "cd '/mnt/c/Users/Rimmel Botond/Documents/bme/hatodikfelev/onlab/moq-rs' && echo 'Cross-Layer Demo Monitor - Press Ctrl+C to exit' && sleep 10 && watch -n 1 'echo \"📊 Cross-Layer Status at \$(date)\" && echo \"========================\" && ps aux | grep -E \"(moq-relay|moq-pub|moq-sub)\" | grep -v grep'" Enter

    # Attach to session
    tmux attach-session -t cross_layer_demo
}

run_demo() {
    echo "🧪 Running Cross-Layer Demo Example..."
    echo ""
    cd "/mnt/c/Users/Rimmel Botond/Documents/bme/hatodikfelev/onlab/moq-rs"
    cargo run --example cross_layer_demo
}

run_monitor() {
    echo "📊 Real-time Cross-Layer Monitoring"
    echo "==================================="
    echo "This will show live bandwidth and cross-layer metrics"
    echo "Press Ctrl+C to exit"
    echo ""

    while true; do
        clear
        echo "📊 Cross-Layer Monitoring - $(date)"
        echo "=================================="
        echo ""

        # Check if processes are running
        echo "🏃 Running Processes:"
        ps aux | grep -E "(moq-relay|moq-pub|moq-sub)" | grep -v grep | awk '{print "  " $11 " (PID: " $2 ")"}'
        echo ""

        echo "📡 Recent Bandwidth Logs (last 5 entries):"
        journalctl --since "1 minute ago" | grep -E "(BANDWIDTH|Smart CC|Cross-layer)" | tail -5 || echo "  No recent bandwidth logs found"
        echo ""

        echo "🔄 Refreshing in 3 seconds... (Ctrl+C to exit)"
        sleep 3
    done
}

# Change to project directory
cd "/mnt/c/Users/Rimmel Botond/Documents/bme/hatodikfelev/onlab/moq-rs"

# Make scripts executable
chmod +x dev/relay_cross_layer dev/pub_cross_layer dev/sub_cross_layer

case "${1:-}" in
    relay)
        run_relay
        ;;
    pub)
        run_pub
        ;;
    sub)
        run_sub
        ;;
    all)
        run_all
        ;;
    demo)
        run_demo
        ;;
    monitor)
        run_monitor
        ;;
    *)
        show_usage
        exit 1
        ;;
esac
