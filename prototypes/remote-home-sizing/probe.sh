#!/usr/bin/env bash
# PROTOTYPE (throwaway): real tmux evidence for non-owner sizing. Run: bash probe.sh
t(){ tmux -L aoeproto "$@"; }; t kill-server 2>/dev/null
t new-session -d -s a -x 200 -y 50 'for i in $(seq 1 60); do echo "line $i: $(printf "x%.0s" $(seq 1 150))"; done; sleep 600'
t set -g window-size manual; t resize-window -t a -x 200 -y 50; sleep 0.5
w(){ t capture-pane -p -t a | awk '{print length($0)}' | sort -n | tail -1; }
echo "owner 200x50: widest captured line = $(w)"
t resize-window -t a -x 60 -y 15
echo "claim+resize 60x15: window=$(t display -p -t a '#{window_width}x#{window_height}') widest=$(w) history=$(t display -p -t a '#{history_size}')"
t resize-window -t a -x 200 -y 50
echo "restored 200x50: widest=$(w)"
t kill-server
