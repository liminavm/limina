// SPDX-License-Identifier: GPL-2.0
/*
 * mmio_cost.ko — what CPPC's frequency-invariance engine (FIE) would cost a limina guest if its
 * delivered/reference counters were MMIO registers. Every guest MMIO read is a stage-2 data abort,
 * i.e. a VM exit to the VMM on HVF.
 *
 *   mode=measure : time `iters` back-to-back reads of the MMIO register at `addr`, and of a RAM
 *                  word, on the current CPU; repeat `reps` times; print ns/read for each repeat.
 *                  Also prints the raw and sanitised ID_AA64PFR0_EL1 (AMU is bits 47:44).
 *   mode=tick    : on every online CPU, an hrtimer (hard-irq, pinned) fires every `period_us` and
 *                  does `nreads` reads — the shape of the master-branch cppc_cpufreq FIE, which
 *                  reads non-PCC counters straight from the scheduler tick.
 *   mode=kthread : the same hrtimer only queues an irq_work, which queues a kthread_work on ONE
 *                  SCHED_DEADLINE kthread that does the reads — the shape of the v6.19 FIE
 *                  (cppc_scale_freq_tick -> cppc_irq_work -> kworker_fie).
 *   ram=1        : in tick/kthread modes, read a RAM word instead of `addr` (the control arm:
 *                  same interrupt/wakeup cost, no exit).
 *
 * Counts are printed at rmmod.
 */
#include <linux/module.h>
#include <linux/io.h>
#include <linux/hrtimer.h>
#include <linux/irq_work.h>
#include <linux/kthread.h>
#include <linux/ktime.h>
#include <linux/percpu.h>
#include <linux/sched.h>
#include <uapi/linux/sched/types.h>
#include <linux/slab.h>
#include <asm/sysreg.h>
#include <asm/cpufeature.h>

static char *mode = "measure";
module_param(mode, charp, 0444);
static unsigned long addr = 0x0a004000; /* virtio-mmio MagicValue: read-only, no side effects */
module_param(addr, ulong, 0444);
static unsigned int iters = 20000;
module_param(iters, uint, 0444);
static unsigned int reps = 7;
module_param(reps, uint, 0444);
static unsigned int period_us = 1000;
module_param(period_us, uint, 0444);
static unsigned int nreads = 2;
module_param(nreads, uint, 0444);
static bool ram;
module_param(ram, bool, 0444);

static void __iomem *mmio;
static u32 *ramword;
static ktime_t period;

struct pc {
	struct hrtimer t;
	struct irq_work iw;
	struct kthread_work kw;
	u64 fires, reads;
	u32 sink;
	int cpu;
};
static DEFINE_PER_CPU(struct pc, pcs);
static struct kthread_worker *kworker;
static bool tick_mode, kthread_mode;

static __always_inline void do_reads(struct pc *p)
{
	unsigned int i;
	u32 s = 0;

	for (i = 0; i < nreads; i++)
		s += ram ? READ_ONCE(*ramword) : readl_relaxed(mmio);
	p->sink += s;
	p->reads += nreads;
}

static void kw_fn(struct kthread_work *w)
{
	do_reads(container_of(w, struct pc, kw));
}

static void iw_fn(struct irq_work *w)
{
	kthread_queue_work(kworker, &container_of(w, struct pc, iw)->kw);
}

static enum hrtimer_restart t_fn(struct hrtimer *t)
{
	struct pc *p = container_of(t, struct pc, t);

	p->fires++;
	if (kthread_mode)
		irq_work_queue(&p->iw);
	else
		do_reads(p);
	hrtimer_forward_now(t, period);
	return HRTIMER_RESTART;
}

static void start_on_cpu(void *unused)
{
	struct pc *p = this_cpu_ptr(&pcs);

	p->cpu = smp_processor_id();
	hrtimer_setup(&p->t, t_fn, CLOCK_MONOTONIC, HRTIMER_MODE_REL_PINNED_HARD);
	hrtimer_start(&p->t, period, HRTIMER_MODE_REL_PINNED_HARD);
}

static void measure(void)
{
	u64 id_raw = read_sysreg_s(SYS_ID_AA64PFR0_EL1);
	u64 id_san = read_sanitised_ftr_reg(SYS_ID_AA64PFR0_EL1);
	unsigned int r, i;
	u32 s = 0;

	pr_info("mmio_cost: ID_AA64PFR0_EL1 raw=%#llx sanitised=%#llx AMU(47:44) raw=%llu sanitised=%llu\n",
		id_raw, id_san, (id_raw >> 44) & 0xf, (id_san >> 44) & 0xf);
	for (r = 0; r < reps; r++) {
		ktime_t t0, t1, t2;

		preempt_disable();
		t0 = ktime_get();
		for (i = 0; i < iters; i++)
			s += readl_relaxed(mmio);
		t1 = ktime_get();
		for (i = 0; i < iters; i++)
			s += READ_ONCE(*ramword);
		t2 = ktime_get();
		preempt_enable();
		pr_info("mmio_cost: cpu=%d rep=%u iters=%u mmio_ns_per_read=%llu.%03llu ram_ns_per_read=%llu.%03llu\n",
			raw_smp_processor_id(), r, iters,
			ktime_to_ns(ktime_sub(t1, t0)) / iters,
			(ktime_to_ns(ktime_sub(t1, t0)) * 1000 / iters) % 1000,
			ktime_to_ns(ktime_sub(t2, t1)) / iters,
			(ktime_to_ns(ktime_sub(t2, t1)) * 1000 / iters) % 1000);
		cond_resched();
	}
	pr_info("mmio_cost: sink=%u magic=%#x\n", s, readl_relaxed(mmio));
}

static int __init mc_init(void)
{
	int cpu;

	mmio = ioremap(addr, 4);
	ramword = kzalloc(64, GFP_KERNEL);
	if (!mmio || !ramword)
		return -ENOMEM;
	period = us_to_ktime(period_us);
	tick_mode = !strcmp(mode, "tick");
	kthread_mode = !strcmp(mode, "kthread");

	if (!tick_mode && !kthread_mode) {
		measure();
		return 0;
	}
	if (kthread_mode) {
		/* the exact attr cppc_freq_invariance_init() gives kworker_fie */
		struct sched_attr attr = {
			.size = sizeof(struct sched_attr),
			.sched_policy = SCHED_DEADLINE,
			.sched_runtime = NSEC_PER_MSEC,
			.sched_deadline = 10 * NSEC_PER_MSEC,
			.sched_period = 10 * NSEC_PER_MSEC,
		};
		kworker = kthread_run_worker(0, "mmio_cost_fie");
		if (IS_ERR(kworker))
			return PTR_ERR(kworker);
		if (sched_setattr_nocheck(kworker->task, &attr))
			pr_warn("mmio_cost: SCHED_DEADLINE refused\n");
	}
	for_each_online_cpu(cpu) {
		struct pc *p = per_cpu_ptr(&pcs, cpu);

		init_irq_work(&p->iw, iw_fn);
		kthread_init_work(&p->kw, kw_fn);
		smp_call_function_single(cpu, start_on_cpu, NULL, 1);
	}
	pr_info("mmio_cost: %s mode, period %u us, %u %s reads per fire\n", mode, period_us,
		nreads, ram ? "RAM" : "MMIO");
	return 0;
}

static void __exit mc_exit(void)
{
	int cpu;

	if (tick_mode || kthread_mode) {
		for_each_online_cpu(cpu) {
			struct pc *p = per_cpu_ptr(&pcs, cpu);

			hrtimer_cancel(&p->t);
			irq_work_sync(&p->iw);
		}
		if (kworker) {
			kthread_flush_worker(kworker);
			kthread_destroy_worker(kworker);
		}
		for_each_online_cpu(cpu) {
			struct pc *p = per_cpu_ptr(&pcs, cpu);

			pr_info("mmio_cost: cpu=%d fires=%llu reads=%llu\n", cpu, p->fires, p->reads);
		}
	}
	iounmap(mmio);
	kfree(ramword);
}

module_init(mc_init);
module_exit(mc_exit);
MODULE_LICENSE("GPL");
