# gdb -q -batch -x hang.gdb --args ./zink-unflushed-wait 120   (with HANG_TRAP=1)
# runs the stress; on a hang the program raises SIGTRAP and the waiter's
# zink_batch_usage is printed (needs Mesa built with debug info)
set pagination off
set debuginfod enabled off
run
source dump-waiter.py
