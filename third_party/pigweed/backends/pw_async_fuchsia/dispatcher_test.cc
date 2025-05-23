// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "pw_async_fuchsia/dispatcher.h"

#include <lib/async-testing/test_loop.h>

#include "pw_async_fuchsia/util.h"
#include "pw_unit_test/framework.h"

#define ASSERT_OK(status) ASSERT_EQ(OkStatus(), status)
#define ASSERT_CANCELLED(status) ASSERT_EQ(Status::Cancelled(), status)

using namespace std::chrono_literals;

namespace pw::async_fuchsia {

class DispatcherFuchsiaTest : public ::testing::Test {
 public:
  async_dispatcher_t* dispatcher() { return loop_.dispatcher(); }
  void RunLoopUntilIdle() { loop_.RunUntilIdle(); }
  void RunLoopFor(zx::duration duration) { loop_.RunFor(duration); }

 private:
  ::async::TestLoop loop_;
};

TEST_F(DispatcherFuchsiaTest, TimeConversions) {
  zx::time time{timespec{123, 456}};
  chrono::SystemClock::time_point tp = pw::async_fuchsia::ZxTimeToTimepoint(time);
  EXPECT_EQ(tp.time_since_epoch(), 123s + 456ns);
  EXPECT_EQ(pw::async_fuchsia::TimepointToZxTime(tp), time);
}

TEST_F(DispatcherFuchsiaTest, Basic) {
  FuchsiaDispatcher fuchsia_dispatcher(dispatcher());

  bool set = false;
  async::Task task([&set](async::Context& ctx, Status status) {
    ASSERT_OK(status);
    set = true;
  });
  fuchsia_dispatcher.Post(task);

  RunLoopUntilIdle();
  EXPECT_TRUE(set);
}

TEST_F(DispatcherFuchsiaTest, DelayedTasks) {
  FuchsiaDispatcher fuchsia_dispatcher(dispatcher());

  int c = 0;
  async::Task first([&c](async::Context& ctx, Status status) {
    ASSERT_OK(status);
    c = c * 10 + 1;
  });
  async::Task second([&c](async::Context& ctx, Status status) {
    ASSERT_OK(status);
    c = c * 10 + 2;
  });
  async::Task third([&c](async::Context& ctx, Status status) {
    ASSERT_OK(status);
    c = c * 10 + 3;
  });

  fuchsia_dispatcher.PostAfter(third, 20ms);
  fuchsia_dispatcher.PostAfter(first, 5ms);
  fuchsia_dispatcher.PostAfter(second, 10ms);

  RunLoopFor(zx::msec(25));
  EXPECT_EQ(c, 123);
}

TEST_F(DispatcherFuchsiaTest, CancelTask) {
  FuchsiaDispatcher fuchsia_dispatcher(dispatcher());

  async::Task task([](async::Context& ctx, Status status) { FAIL(); });
  fuchsia_dispatcher.Post(task);
  EXPECT_TRUE(fuchsia_dispatcher.Cancel(task));

  RunLoopUntilIdle();
}

class DestructionChecker {
 public:
  explicit DestructionChecker(bool* flag) : flag_(flag) {}
  DestructionChecker(DestructionChecker&& other) {
    flag_ = other.flag_;
    other.flag_ = nullptr;
  }
  ~DestructionChecker() {
    if (flag_) {
      *flag_ = true;
    }
  }

 private:
  bool* flag_;
};

TEST_F(DispatcherFuchsiaTest, HeapAllocatedTasks) {
  FuchsiaDispatcher fuchsia_dispatcher(dispatcher());

  int c = 0;
  for (int i = 0; i < 3; i++) {
    pw::async_fuchsia::Post(&fuchsia_dispatcher, [&c](async::Context& ctx, Status status) {
      ASSERT_OK(status);
      c++;
    });
  }

  EXPECT_EQ(c, 0);
  RunLoopUntilIdle();
  EXPECT_EQ(c, 3);

  // Test that the lambda is destroyed after being called.
  bool flag = false;
  pw::async_fuchsia::Post(&fuchsia_dispatcher, [checker = DestructionChecker(&flag)](
                                                   async::Context& ctx, Status status) {});
  EXPECT_FALSE(flag);
  RunLoopUntilIdle();
  EXPECT_TRUE(flag);
}

TEST_F(DispatcherFuchsiaTest, ChainedTasks) {
  FuchsiaDispatcher fuchsia_dispatcher(dispatcher());

  int c = 0;

  pw::async_fuchsia::Post(&fuchsia_dispatcher, [&c](async::Context& ctx, Status status) {
    ASSERT_OK(status);
    c++;
    pw::async_fuchsia::Post(ctx.dispatcher, [&c](async::Context& ctx, Status status) {
      ASSERT_OK(status);
      c++;
      pw::async_fuchsia::Post(ctx.dispatcher, [&c](async::Context& ctx, Status status) {
        ASSERT_OK(status);
        c++;
      });
    });
  });

  RunLoopUntilIdle();
  EXPECT_EQ(c, 3);
}

}  // namespace pw::async_fuchsia
