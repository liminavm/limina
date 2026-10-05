# gdb: for each thread inside zink_batch_usage_unflushed_wait, print the usage it sleeps on
import gdb
for t in gdb.selected_inferior().threads():
    t.switch()
    f = gdb.newest_frame()
    while f:
        if f.name() == "zink_batch_usage_unflushed_wait":
            try:
                u = f.read_var("u").dereference()
                print("WAITER thread %d: unflushed=%s usage=%s submit_count=%s" % (t.num, u["unflushed"], u["usage"], u["submit_count"]))
            except Exception as e:
                print("WAITER thread %d: %s" % (t.num, e))
            break
        f = f.older()
