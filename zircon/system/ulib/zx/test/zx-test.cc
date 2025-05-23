// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <assert.h>
#include <lib/fit/defer.h>
#include <lib/fzl/time.h>
#include <lib/zx/bti.h>
#include <lib/zx/channel.h>
#include <lib/zx/event.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/handle.h>
#include <lib/zx/iob.h>
#include <lib/zx/iommu.h>
#include <lib/zx/job.h>
#include <lib/zx/port.h>
#include <lib/zx/process.h>
#include <lib/zx/profile.h>
#include <lib/zx/result.h>
#include <lib/zx/socket.h>
#include <lib/zx/suspend_token.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <lib/zx/vmar.h>
#include <stdint.h>
#include <stdio.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/compiler.h>
#include <zircon/limits.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/iob.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/port.h>
#include <zircon/threads.h>
#include <zircon/types.h>

#include <utility>

#include <zxtest/zxtest.h>

#include "util.h"

namespace {

zx_status_t validate_handle(zx_handle_t handle) {
  return zx_object_get_info(handle, ZX_INFO_HANDLE_VALID, nullptr, 0, 0u, nullptr);
}

TEST(ZxTestCase, HandleInvalid) {
  zx::handle handle;
  // A default constructed handle is invalid.
  ASSERT_EQ(handle.release(), ZX_HANDLE_INVALID);
}

TEST(ZxTestCase, HandleClose) {
  zx_handle_t raw_event;
  ASSERT_OK(zx_event_create(0u, &raw_event));
  ASSERT_OK(validate_handle(raw_event));
  {
    zx::handle handle(raw_event);
  }
  // Make sure the handle was closed.
  ASSERT_EQ(validate_handle(raw_event), ZX_ERR_BAD_HANDLE);
}

TEST(ZxTestCase, HandleMove) {
  zx::event event;
  // Check move semantics.
  ASSERT_OK(zx::event::create(0u, &event));
  zx::handle handle(std::move(event));
  ASSERT_EQ(event.release(), ZX_HANDLE_INVALID);
  ASSERT_OK(validate_handle(handle.get()));
}

TEST(ZxTestCase, HandleDuplicate) {
  zx_handle_t raw_event;
  zx::handle dup;
  ASSERT_OK(zx_event_create(0u, &raw_event));
  zx::handle handle(raw_event);
  ASSERT_OK(handle.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup));
  // The duplicate must be valid as well as the original.
  ASSERT_OK(validate_handle(dup.get()));
  ASSERT_OK(validate_handle(raw_event));
}

TEST(ZxTestCase, HandleReplace) {
  zx_handle_t raw_event;
  zx::handle rep;
  ASSERT_OK(zx_event_create(0u, &raw_event));
  {
    zx::handle handle(raw_event);
    ASSERT_OK(handle.replace(ZX_RIGHT_SAME_RIGHTS, &rep));
    ASSERT_EQ(handle.release(), ZX_HANDLE_INVALID);
  }
  // The original shoould be invalid and the replacement should be valid.
  ASSERT_EQ(validate_handle(raw_event), ZX_ERR_BAD_HANDLE);
  ASSERT_OK(validate_handle(rep.get()));
}

TEST(ZxTestCase, GetInfo) {
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(1, 0u, &vmo));

  // zx::vmo is just an easy object to create; this is really a test of zx::object_base.
  const zx::object_base& object = vmo;
  zx_info_handle_count_t info;
  EXPECT_OK(object.get_info(ZX_INFO_HANDLE_COUNT, &info, sizeof(info), nullptr, nullptr));
  EXPECT_EQ(info.handle_count, 1);
}

TEST(ZxTestCase, SetGetProperty) {
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(1, 0u, &vmo));

  // zx::vmo is just an easy object to create; this is really a test of zx::object_base.
  const char name[] = "a great maximum length vmo name";
  const zx::object_base& object = vmo;
  EXPECT_OK(object.set_property(ZX_PROP_NAME, name, sizeof(name)));

  char read_name[ZX_MAX_NAME_LEN];
  EXPECT_OK(object.get_property(ZX_PROP_NAME, read_name, sizeof(read_name)));
  EXPECT_STREQ(name, read_name);
}

TEST(ZxTestCase, Event) {
  zx::event event;
  ASSERT_OK(zx::event::create(0u, &event));
  ASSERT_OK(validate_handle(event.get()));
  // TODO(cpu): test more.
}

TEST(ZxTestCase, EventDuplicate) {
  zx::event event;
  zx::event dup;
  ASSERT_OK(zx::event::create(0u, &event));
  ASSERT_OK(event.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup));
  // The duplicate must be valid as well as the original.
  ASSERT_OK(validate_handle(dup.get()));
  ASSERT_OK(validate_handle(event.get()));
}

TEST(ZxTestCase, BtiCompilation) {
  zx::bti bti;
  // TODO(teisenbe): test more.
}

TEST(ZxTestCase, PmtCompilation) {
  zx::pmt pmt;
  // TODO(teisenbe): test more.
}

TEST(ZxTestCase, IommuCompilation) {
  zx::iommu iommu;
  // TODO(teisenbe): test more.
}

TEST(ZxTestCase, Channel) {
  zx::channel channel[2];
  ASSERT_OK(zx::channel::create(0u, &channel[0], &channel[1]));
  ASSERT_OK(validate_handle(channel[0].get()));
  ASSERT_OK(validate_handle(channel[1].get()));
  // TODO(cpu): test more.
}

TEST(ZxTestCase, ChannelRw) {
  zx::eventpair eventpair[2];
  ASSERT_OK(zx::eventpair::create(0u, &eventpair[0], &eventpair[1]));

  zx::channel channel[2];
  ASSERT_OK(zx::channel::create(0u, &channel[0], &channel[1]));

  zx_handle_t handles[2] = {eventpair[0].release(), eventpair[1].release()};

  zx_handle_t recv[2] = {0};

  ASSERT_OK(channel[0].write(0u, nullptr, 0u, handles, 2));
  ASSERT_OK(channel[1].read(0u, nullptr, recv, 0u, 2, nullptr, nullptr));

  ASSERT_OK(zx_handle_close(recv[0]));
  ASSERT_OK(zx_handle_close(recv[1]));
}

TEST(ZxTestCase, ChannelRwEtc) {
  zx::eventpair eventpair[2];
  ASSERT_OK(zx::eventpair::create(0u, &eventpair[0], &eventpair[1]));

  zx::channel channel[2];
  ASSERT_OK(zx::channel::create(0u, &channel[0], &channel[1]));

  zx_handle_t handles[2] = {eventpair[0].release(), eventpair[1].release()};

  zx_handle_info_t recv[2] = {{}};
  uint32_t h_count = 0;

  ASSERT_OK(channel[0].write(0u, nullptr, 0u, handles, 2));
  ASSERT_OK(channel[1].read_etc(0u, nullptr, recv, 0u, 2, nullptr, &h_count));

  ASSERT_EQ(h_count, 2u);
  ASSERT_EQ(recv[0].type, ZX_OBJ_TYPE_EVENTPAIR);
  ASSERT_EQ(recv[1].type, ZX_OBJ_TYPE_EVENTPAIR);

  ASSERT_OK(zx_handle_close(recv[0].handle));
  ASSERT_OK(zx_handle_close(recv[1].handle));
}

TEST(ZxTestCase, Socket) {
  zx::socket socket[2];
  ASSERT_OK(zx::socket::create(0u, &socket[0], &socket[1]));
  ASSERT_OK(validate_handle(socket[0].get()));
  ASSERT_OK(validate_handle(socket[1].get()));
  // TODO(cpu): test more.
}

TEST(ZxTestCase, EventPair) {
  zx::eventpair eventpair[2];
  ASSERT_OK(zx::eventpair::create(0u, &eventpair[0], &eventpair[1]));
  ASSERT_OK(validate_handle(eventpair[0].get()));
  ASSERT_OK(validate_handle(eventpair[1].get()));
  // TODO(cpu): test more.
}

TEST(ZxTestCase, Vmar) {
  zx::vmar vmar;
  const size_t size = getpagesize();
  uintptr_t addr;
  ASSERT_OK(zx::vmar::root_self()->allocate(ZX_VM_CAN_MAP_READ, 0u, size, &vmar, &addr));
  ASSERT_OK(validate_handle(vmar.get()));
  ASSERT_OK(vmar.destroy());
  // TODO(teisenbe): test more.
}

TEST(ZxTestCase, Port) {
  zx::port port;
  ASSERT_OK(zx::port::create(0, &port));
  ASSERT_OK(validate_handle(port.get()));

  zx::channel channel[2];
  auto key = 1111ull;
  ASSERT_OK(zx::channel::create(0u, &channel[0], &channel[1]));
  ASSERT_OK(channel[0].wait_async(port, key, ZX_CHANNEL_READABLE, 0));
  ASSERT_OK(channel[1].write(0u, "12345", 5, nullptr, 0u));

  zx_port_packet_t packet = {};
  ASSERT_OK(port.wait(zx::time(), &packet));
  ASSERT_EQ(packet.key, key);
  ASSERT_EQ(packet.type, ZX_PKT_TYPE_SIGNAL_ONE);
  ASSERT_EQ(packet.signal.count, 1u);
}

TEST(ZxTestCase, TimeConstruction) {
  // time construction
  ASSERT_EQ(zx::time().get(), 0);
  ASSERT_EQ(zx::time::infinite().get(), ZX_TIME_INFINITE);
  ASSERT_EQ(zx::time(-1).get(), -1);
  ASSERT_EQ(zx::time(ZX_TIME_INFINITE_PAST).get(), ZX_TIME_INFINITE_PAST);
  ASSERT_EQ(zx::time(timespec{123, 456}).get(), ZX_SEC(123) + ZX_NSEC(456));
}

TEST(ZxTestCase, TimeConversions) {
  const timespec ts = zx::time(timespec{123, 456}).to_timespec();
  ASSERT_EQ(ts.tv_sec, 123);
  ASSERT_EQ(ts.tv_nsec, 456);
}

TEST(ZxTestCase, DurationConstruction) {
  // duration construction
  ASSERT_EQ(zx::duration().get(), 0);
  ASSERT_EQ(zx::duration::infinite().get(), ZX_TIME_INFINITE);
  ASSERT_EQ(zx::duration(-1).get(), -1);
  ASSERT_EQ(zx::duration(ZX_TIME_INFINITE_PAST).get(), ZX_TIME_INFINITE_PAST);
  ASSERT_EQ(zx::duration(timespec{123, 456}).get(), ZX_SEC(123) + ZX_NSEC(456));
}

TEST(ZxTestCase, DurationConversions) {
  // duration to/from nsec, usec, msec, etc.
  ASSERT_EQ(zx::nsec(-10).get(), ZX_NSEC(-10));
  ASSERT_EQ(zx::nsec(-10).to_nsecs(), -10);
  ASSERT_EQ(zx::nsec(10).get(), ZX_NSEC(10));
  ASSERT_EQ(zx::nsec(10).to_nsecs(), 10);
  ASSERT_EQ(zx::usec(10).get(), ZX_USEC(10));
  ASSERT_EQ(zx::usec(10).to_usecs(), 10);
  ASSERT_EQ(zx::msec(10).get(), ZX_MSEC(10));
  ASSERT_EQ(zx::msec(10).to_msecs(), 10);
  ASSERT_EQ(zx::sec(10).get(), ZX_SEC(10));
  ASSERT_EQ(zx::sec(10).to_secs(), 10);
  ASSERT_EQ(zx::min(10).get(), ZX_MIN(10));
  ASSERT_EQ(zx::min(10).to_mins(), 10);
  ASSERT_EQ(zx::hour(10).get(), ZX_HOUR(10));
  ASSERT_EQ(zx::hour(10).to_hours(), 10);

  const timespec ts = zx::duration(timespec{123, 456}).to_timespec();
  ASSERT_EQ(ts.tv_sec, 123);
  ASSERT_EQ(ts.tv_nsec, 456);

  ASSERT_EQ((zx::time() + zx::usec(19)).get(), ZX_USEC(19));
  ASSERT_EQ((zx::usec(19) + zx::time()).get(), ZX_USEC(19));
  ASSERT_EQ((zx::time::infinite() - zx::time()).get(), ZX_TIME_INFINITE);
  ASSERT_EQ((zx::time::infinite() - zx::time::infinite()).get(), 0);
  ASSERT_EQ((zx::time() + zx::duration::infinite()).get(), ZX_TIME_INFINITE);

  zx::duration d(0u);
  d += zx::nsec(19);
  ASSERT_EQ(d.get(), ZX_NSEC(19));
  d -= zx::nsec(19);
  ASSERT_EQ(d.get(), ZX_NSEC(0));

  d = zx::min(1);
  d *= 19u;
  ASSERT_EQ(d.get(), ZX_MIN(19));
  d /= 19u;
  ASSERT_EQ(d.get(), ZX_MIN(1));

  ASSERT_EQ(zx::sec(19) % zx::sec(7), ZX_SEC(5));

  zx::time t(0u);
  t += zx::msec(19);
  ASSERT_EQ(t.get(), ZX_MSEC(19));
  t -= zx::msec(19);
  ASSERT_EQ(t.get(), ZX_MSEC(0));

  ASSERT_EQ((2 * zx::msec(10)).get(), ZX_MSEC(20));
  ASSERT_EQ((zx::msec(10) * 2).get(), ZX_MSEC(20));
  ASSERT_EQ((-zx::msec(10)).get(), ZX_MSEC(-10));
  ASSERT_EQ((-zx::duration::infinite()).get(), ZX_TIME_INFINITE_PAST + 1);
  ASSERT_EQ((-zx::duration::infinite_past()).get(), ZX_TIME_INFINITE);

  // Just a smoke test
  ASSERT_GE(zx::deadline_after(zx::usec(10)).get(), ZX_USEC(10));
}

TEST(ZxTestCase, TimeNanoSleep) {
  ASSERT_OK(zx::nanosleep(zx::time(ZX_TIME_INFINITE_PAST)));
  ASSERT_OK(zx::nanosleep(zx::time(-1)));
  ASSERT_OK(zx::nanosleep(zx::time(0)));
  ASSERT_OK(zx::nanosleep(zx::time(1)));
}

TEST(ZxTestCase, Ticks) {
  // Check that the default constructor initialized to 0.
  ASSERT_EQ(zx::ticks().get(), 0);

  // Sanity check the math operators.
  zx::ticks res;

  // Addition
  res = zx::ticks(5) + zx::ticks(7);
  ASSERT_EQ(res.get(), 12);
  res = zx::ticks(5);
  res += zx::ticks(7);
  ASSERT_EQ(res.get(), 12);

  // Subtraction
  res = zx::ticks(5) - zx::ticks(7);
  ASSERT_EQ(res.get(), -2);
  res = zx::ticks(5);
  res -= zx::ticks(7);
  ASSERT_EQ(res.get(), -2);

  // Multiplication
  res = zx::ticks(7) * 3;
  ASSERT_EQ(res.get(), 21);
  res = zx::ticks(7);
  res *= 3;
  ASSERT_EQ(res.get(), 21);

  // Division
  res = zx::ticks(25) / 7;
  ASSERT_EQ(res.get(), 3);
  res = zx::ticks(25);
  res /= 7;
  ASSERT_EQ(res.get(), 3);

  // Modulus
  res = zx::ticks(25) % 7;
  ASSERT_EQ(res.get(), 4);
  res = zx::ticks(25);
  res %= 7;
  ASSERT_EQ(res.get(), 4);

  // Test basic comparison, also set up for testing monotonicity.
  zx::ticks before = zx::ticks::now();
  ASSERT_GT(before.get(), 0);
  zx::ticks after = before + zx::ticks(1);

  ASSERT_LT(before.get(), after.get());
  ASSERT_TRUE(before < after);
  ASSERT_TRUE(before <= after);
  ASSERT_TRUE(before <= before);

  ASSERT_TRUE(after > before);
  ASSERT_TRUE(after >= before);
  ASSERT_TRUE(after >= after);

  ASSERT_TRUE(before == before);
  ASSERT_TRUE(before != after);

  after -= zx::ticks(1);
  ASSERT_EQ(before.get(), after.get());
  ASSERT_TRUE(before == after);

  // Make sure that zx::ticks TPS agrees with the syscall.
  ASSERT_EQ(zx::ticks::per_second().get(), zx_ticks_per_second());

  // Compare a duration (nanoseconds) with the ticks equivalent.
  zx::ticks second = zx::ticks::per_second();
  ASSERT_EQ(fzl::TicksToNs(second).get(), zx::sec(1).get());
  ASSERT_TRUE(fzl::TicksToNs(second) == zx::sec(1));

  // Make sure that the libzx ticks operators saturate properly, instead of
  // overflowing.  Start with addition.
  constexpr zx::ticks ALMOST_MAX = zx::ticks(std::numeric_limits<zx_ticks_t>::max() - 5);
  constexpr zx::ticks ALMOST_MIN = zx::ticks(std::numeric_limits<zx_ticks_t>::min() + 5);
  constexpr zx::ticks ABSOLUTE_MIN = zx::ticks(std::numeric_limits<zx_ticks_t>::min());
  constexpr zx::ticks ZERO = zx::ticks(0);

  res = ALMOST_MAX + zx::ticks(10);
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());
  res = ALMOST_MAX;
  res += zx::ticks(10);
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());

  res = ALMOST_MIN + zx::ticks(-10);
  ASSERT_EQ(res.get(), zx::ticks::infinite_past().get());
  res = ALMOST_MIN;
  res += zx::ticks(-10);
  ASSERT_EQ(res.get(), zx::ticks::infinite_past().get());

  // Now, subtraction
  res = ALMOST_MIN - zx::ticks(10);
  ASSERT_EQ(res.get(), zx::ticks::infinite_past().get());
  res = ALMOST_MIN;
  res -= zx::ticks(10);
  ASSERT_EQ(res.get(), zx::ticks::infinite_past().get());

  res = ALMOST_MAX - zx::ticks(-10);
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());
  res = ALMOST_MAX;
  res -= zx::ticks(-10);
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());

  res = ZERO - ABSOLUTE_MIN;
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());
  res = ZERO;
  res -= ABSOLUTE_MIN;
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());

  // Finally, multiplication
  res = ALMOST_MAX * 2;
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());
  res = ALMOST_MAX;
  res *= 2;
  ASSERT_EQ(res.get(), zx::ticks::infinite().get());

  res = ALMOST_MIN * 2;
  ASSERT_EQ(res.get(), zx::ticks::infinite_past().get());
  res = ALMOST_MIN;
  res *= 2;
  ASSERT_EQ(res.get(), zx::ticks::infinite_past().get());

  // Hopefully, we haven't moved backwards in time.
  after = zx::ticks::now();
  ASSERT_LE(before.get(), after.get());
  ASSERT_TRUE(before <= after);
}

template <typename T>
void IsValidHandle(const T& p) {
  ASSERT_TRUE(static_cast<bool>(p), "invalid handle");
}

TEST(ZxTestCase, ThreadSelf) {
  zx_handle_t raw = zx_thread_self();
  ASSERT_OK(validate_handle(raw));

  ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::thread>(*zx::thread::self()));
  EXPECT_OK(validate_handle(raw));

  // This does not compile:
  // const zx::thread self = zx::thread::self();
}

TEST(ZxTestCase, ThreadCreate) {
  zx::thread thread;
  const char* name = "test thread";
  ASSERT_OK(zx::thread::create(*zx::process::self(), name, sizeof(name), 0u, &thread));
  EXPECT_TRUE(thread.is_valid());
  EXPECT_OK(validate_handle(thread.get()));
}

TEST(ZxTestCase, ThreadSetProfile) {
  zx::thread thread;
  const char* name = "test thread";
  ASSERT_OK(zx::thread::create(*zx::process::self(), name, sizeof(name), 0u, &thread));

  zx::profile profile;
  zx_profile_info_t info = {};
  info.flags = ZX_PROFILE_INFO_FLAG_PRIORITY;
  info.priority = ZX_PRIORITY_LOWEST;
  ASSERT_OK(zx::profile::create(GetProfileResource(), 0u, &info, &profile));
  EXPECT_OK(thread.set_profile(profile, 0u));
}

int thread_suspend_test_fn(void* arg) {
  zx_handle_t* event_wait = reinterpret_cast<zx_handle_t*>(arg);
  zx_object_wait_one(*event_wait, ZX_USER_SIGNAL_0, zx::time::infinite().get(), nullptr);
  return 0;
}

TEST(ZxTestCase, ThreadSuspend) {
  zx_handle_t event_wait;
  ASSERT_OK(zx_event_create(0, &event_wait));
  auto cleanup = fit::defer([event_wait] { ASSERT_OK(zx_handle_close(event_wait)); });

  // We can't use syscalls to create the thread here because we are running and exiting the
  // thread. Going through the C APIs is the easiest way to ensure that ASAN and other
  // sanitizers will be happy.
  thrd_t thread;
  ASSERT_EQ(thrd_create(&thread, thread_suspend_test_fn, reinterpret_cast<void*>(&event_wait)),
            thrd_success);

  zx::unowned_thread zx_thread(thrd_get_zx_handle(thread));

  zx::suspend_token suspend;
  EXPECT_OK(zx_thread->suspend(&suspend));
  EXPECT_TRUE(suspend);
  ASSERT_OK(zx_thread->wait_one(ZX_THREAD_SUSPENDED, zx::time::infinite(), nullptr));

  suspend.reset();
  ASSERT_OK(zx_object_signal(event_wait, 0, ZX_USER_SIGNAL_0));

  int result = 0;
  ASSERT_EQ(thrd_join(thread, &result), thrd_success);
  ASSERT_EQ(result, 0);
}

TEST(ZxTestCase, ProcessSelf) {
  zx_handle_t raw = zx_process_self();
  ASSERT_OK(validate_handle(raw));

  ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::process>(*zx::process::self()));
  EXPECT_OK(validate_handle(raw));

  // This does not compile:
  // const zx::process self = zx::process::self();
}

TEST(ZxTestCase, VmarRootSelf) {
  zx_handle_t raw = zx_vmar_root_self();
  ASSERT_OK(validate_handle(raw));

  ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::vmar>(*zx::vmar::root_self()));
  EXPECT_OK(validate_handle(raw));

  // This does not compile:
  // const zx::vmar root_self = zx::vmar::root_self();
}

TEST(ZxTestCase, JobDefault) {
  zx_handle_t raw = zx_job_default();
  ASSERT_OK(validate_handle(raw));

  ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::job>(*zx::job::default_job()));
  EXPECT_OK(validate_handle(raw));

  // This does not compile:
  // const zx::job default_job = zx::job::default_job();
}

bool takes_any_handle(const zx::handle& handle) { return handle.is_valid(); }

TEST(ZxTestCase, HandleConversion) {
  EXPECT_TRUE(takes_any_handle(*zx::unowned_handle(zx_thread_self())));
  ASSERT_OK(validate_handle(zx_thread_self()));
}

TEST(ZxTestCase, Unowned) {
  // Create a handle to test with.
  zx::event handle;
  ASSERT_OK(zx::event::create(0, &handle));
  ASSERT_OK(validate_handle(handle.get()));

  // Verify that unowned<T>(zx_handle_t) doesn't close handle on teardown.
  {
    zx::unowned<zx::event> unowned(handle.get());
    EXPECT_EQ(unowned->get(), handle.get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Verify that unowned<T>(const T&) doesn't close handle on teardown.
  {
    zx::unowned<zx::event> unowned(handle);
    EXPECT_EQ(unowned->get(), handle.get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Verify that unowned<T>(const unowned<T>&) doesn't close on teardown.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    zx::unowned<zx::event> unowned2(unowned);
    EXPECT_EQ(unowned->get(), unowned2->get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned2));
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Verify copy-assignment from unowned<> to unowned<> doesn't close.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    zx::unowned<zx::event> unowned2;
    ASSERT_FALSE(unowned2->is_valid());

    const zx::unowned<zx::event>& assign_ref = unowned2 = unowned;
    EXPECT_EQ(assign_ref->get(), unowned2->get());
    EXPECT_EQ(unowned->get(), unowned2->get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned2));
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Verify move from unowned<> to unowned<> doesn't close on teardown.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    zx::unowned<zx::event> unowned2(static_cast<zx::unowned<zx::event>&&>(unowned));
    EXPECT_EQ(unowned2->get(), handle.get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned2));
    EXPECT_FALSE(unowned->is_valid());
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Verify move-assignment from unowned<> to unowned<> doesn't close.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    zx::unowned<zx::event> unowned2;
    ASSERT_FALSE(unowned2->is_valid());

    const zx::unowned<zx::event>& assign_ref = unowned2 =
        static_cast<zx::unowned<zx::event>&&>(unowned);
    EXPECT_EQ(assign_ref->get(), unowned2->get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned2));
    EXPECT_FALSE(unowned->is_valid());
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Verify move-assignment into non-empty unowned<>  doesn't close.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    zx::unowned<zx::event> unowned2(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned2));

    unowned2 = static_cast<zx::unowned<zx::event>&&>(unowned);
    EXPECT_EQ(unowned2->get(), handle.get());
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned2));
    EXPECT_FALSE(unowned->is_valid());
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Explicitly verify dereference operator allows methods to be called.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    const zx::event& event_ref = *unowned;
    zx::event duplicate;
    EXPECT_OK(event_ref.duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate));
  }
  ASSERT_OK(validate_handle(handle.get()));

  // Explicitly verify member access operator allows methods to be called.
  {
    zx::unowned<zx::event> unowned(handle);
    ASSERT_NO_FATAL_FAILURE(IsValidHandle<zx::event>(*unowned));

    zx::event duplicate;
    EXPECT_OK(unowned->duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate));
  }
  ASSERT_OK(validate_handle(handle.get()));
}

TEST(ZxTestCase, GetChild) {
  {
    // Verify handle and job overrides of get_child() can find this process
    // by KOID.
    zx_info_handle_basic_t info = {};
    ASSERT_OK(zx_object_get_info(zx_process_self(), ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                 nullptr, nullptr));

    zx::handle as_handle;
    ASSERT_OK(zx::job::default_job()->get_child(info.koid, ZX_RIGHT_SAME_RIGHTS, &as_handle));
    ASSERT_OK(validate_handle(as_handle.get()));

    zx::process as_process;
    ASSERT_OK(zx::job::default_job()->get_child(info.koid, ZX_RIGHT_SAME_RIGHTS, &as_process));
    ASSERT_OK(validate_handle(as_process.get()));
  }

  {
    // Verify handle and thread overrides of get_child() can find this
    // thread by KOID.
    zx_info_handle_basic_t info = {};
    ASSERT_OK(zx_object_get_info(zx_thread_self(), ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                 nullptr, nullptr));

    zx::handle as_handle;
    ASSERT_OK(zx::process::self()->get_child(info.koid, ZX_RIGHT_SAME_RIGHTS, &as_handle));
    ASSERT_OK(validate_handle(as_handle.get()));

    zx::thread as_thread;
    ASSERT_OK(zx::process::self()->get_child(info.koid, ZX_RIGHT_SAME_RIGHTS, &as_thread));
    ASSERT_OK(validate_handle(as_thread.get()));
  }
}

TEST(ZxTestCase, VmoContentSize) {
  zx::vmo vmo;
  constexpr uint32_t options = 0;
  constexpr uint64_t initial_size = 8 * 1024;
  ASSERT_OK(zx::vmo::create(initial_size, options, &vmo));

  uint64_t retrieved_size = 0;
  ASSERT_OK(vmo.get_prop_content_size(&retrieved_size));
  EXPECT_EQ(retrieved_size, initial_size);
  retrieved_size = 0;

  constexpr uint64_t new_size = 500;
  EXPECT_OK(vmo.set_prop_content_size(new_size));

  ASSERT_OK(vmo.get_prop_content_size(&retrieved_size));
  EXPECT_EQ(retrieved_size, new_size);
  retrieved_size = 0;

  ASSERT_OK(zx_object_get_property(vmo.get(), ZX_PROP_VMO_CONTENT_SIZE, &retrieved_size,
                                   sizeof(retrieved_size)));
  EXPECT_EQ(retrieved_size, new_size);
}

class IobMapping {
 public:
  ~IobMapping() { Unmap(); }

  IobMapping(const IobMapping&) = delete;
  IobMapping& operator=(const IobMapping&) = delete;

  IobMapping& operator=(IobMapping&& other) {
    this->Unmap();
    std::swap(addr_, other.addr_);
    std::swap(region_len_, other.region_len_);
    return *this;
  }
  IobMapping(IobMapping&& other) { *this = std::move(other); }

  static zx::result<IobMapping> Create(zx_vm_option_t options, size_t vmar_offset,
                                       const zx::iob& iob_handle, uint32_t region_index,
                                       uint64_t region_offset, size_t region_len) {
    zx_vaddr_t addr{0};
    zx_status_t res = zx::vmar::root_self()->map_iob(options, vmar_offset, iob_handle, region_index,
                                                     region_offset, region_len, &addr);
    if (res != ZX_OK) {
      return zx::error(res);
    }

    return zx::ok(IobMapping{addr, region_len});
  }

  zx_status_t Unmap() {
    if (addr_ != 0) {
      zx_status_t res = zx::vmar::root_self()->unmap(addr_, region_len_);
      addr_ = 0;
      region_len_ = 0;
      return res;
    }
    return ZX_ERR_BAD_STATE;
  }

  zx_vaddr_t addr() const { return addr_; }
  size_t region_len() const { return region_len_; }

 private:
  IobMapping(zx_vaddr_t addr, size_t region_len) : addr_(addr), region_len_(region_len) {}

  zx_vaddr_t addr_{0};
  size_t region_len_{0};
};

TEST(ZxTestCase, IobCreateAndMap) {
  zx::iob iob;
  zx_iob_region_t regions[2] = {
      zx_iob_region_t{
          .type = ZX_IOB_REGION_TYPE_PRIVATE,
          .access = ZX_IOB_ACCESS_EP0_CAN_MAP_READ | ZX_IOB_ACCESS_EP0_CAN_MAP_WRITE,
          .size = ZX_PAGE_SIZE,
          .discipline = zx_iob_discipline_t{.type = ZX_IOB_DISCIPLINE_TYPE_NONE},
          .private_region =
              {
                  .options = 0,
              },
      },
      zx_iob_region_t{
          .type = ZX_IOB_REGION_TYPE_PRIVATE,
          .access = ZX_IOB_ACCESS_EP1_CAN_MAP_READ | ZX_IOB_ACCESS_EP1_CAN_MAP_WRITE,
          .size = ZX_PAGE_SIZE,
          .discipline = zx_iob_discipline_t{.type = ZX_IOB_DISCIPLINE_TYPE_NONE},
          .private_region =
              {
                  .options = 0,
              },
      },
  };
  zx::iob ep0;
  zx::iob ep1;
  ASSERT_OK(zx::iob::create(0, regions, 2, &ep0, &ep1));
  ASSERT_OK(validate_handle(ep0.get()));
  ASSERT_OK(validate_handle(ep1.get()));

  zx::result<IobMapping> mapping0 =
      IobMapping::Create(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0u, ep0, 0, 0, ZX_PAGE_SIZE);
  ASSERT_OK(mapping0.status_value());
  zx::result<IobMapping> mapping1 =
      IobMapping::Create(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0u, ep1, 1, 0, ZX_PAGE_SIZE);
  ASSERT_OK(mapping1.status_value());
}

}  // namespace
