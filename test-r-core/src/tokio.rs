use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::spawn_blocking;

use crate::args::Arguments;
use crate::execution::TestSuiteExecution;
use crate::internal::{DependencyView, RegisteredTest, TestFunction, TestResult};
use crate::output::{test_runner_output, TestRunnerOutput};
use crate::{args, internal};

pub fn test_runner() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_test_runner());
}

async fn async_test_runner() {
    let args = args::Arguments::from_args();
    let output = test_runner_output(&args);

    let registered_tests = internal::REGISTERED_TESTS.lock().unwrap();
    let registered_dependency_constructors =
        internal::REGISTERED_DEPENDENCY_CONSTRUCTORS.lock().unwrap();

    if args.list {
        output.test_list(&registered_tests);
    } else {
        let execution = TestSuiteExecution::construct(
            &args,
            registered_dependency_constructors.as_slice(),
            registered_tests.as_slice(),
        );
        // println!("Execution plan: {execution:?}");

        let count = execution.remaining();
        let results = Arc::new(Mutex::new(Vec::with_capacity(count)));

        output.start_suite(count);

        tokio_scoped::scope(|s| {
            let execution = Arc::new(Mutex::new(execution));
            let threads = args.test_threads().get();
            for _ in 0..threads {
                let execution_clone = execution.clone();
                let output_clone = output.clone();
                let args_clone = args.clone();
                let results_clone = results.clone();
                s.spawn(async move {
                    test_thread(
                        args_clone,
                        execution_clone,
                        output_clone,
                        count,
                        results_clone,
                    )
                    .await
                });
            }
        });

        output.finished_suite(&registered_tests, &results.lock().await);
    }
}

async fn test_thread<'a>(
    args: Arguments,
    execution: Arc<Mutex<TestSuiteExecution<'a>>>,
    output: Arc<dyn TestRunnerOutput>,
    count: usize,
    results: Arc<Mutex<Vec<(RegisteredTest, TestResult)>>>,
) {
    while let Some((registered_test, deps, idx)) = pick_next(&execution).await {
        output.start_running_test(registered_test, idx, count);
        let result = run_test(args.include_ignored, deps, registered_test).await;
        output.finished_running_test(registered_test, idx, count, &result);

        results.lock().await.push((registered_test.clone(), result));
    }
}

async fn pick_next<'a>(
    execution: &Arc<Mutex<TestSuiteExecution<'a>>>,
) -> Option<(
    &'a RegisteredTest,
    Box<dyn DependencyView + Send + Sync>,
    usize,
)> {
    let mut execution = execution.lock().await;
    execution.pick_next().await
}

async fn run_test(
    include_ignored: bool,
    dependency_view: Box<dyn internal::DependencyView + Send + Sync>,
    test: &RegisteredTest,
) -> internal::TestResult {
    if test.is_ignored && !include_ignored {
        internal::TestResult::Ignored
    } else {
        match &test.run {
            TestFunction::Sync(_) => {
                let test_fn = test.run.clone();
                let handle = spawn_blocking(move || {
                    crate::sync::run_sync_test_function(&test_fn, dependency_view)
                });
                handle
                    .await
                    .unwrap_or_else(|join_error| internal::TestResult::Failed {
                        panic: Box::new(join_error),
                    })
            }
            TestFunction::Async(test_fn) => {
                match AssertUnwindSafe(test_fn(dependency_view))
                    .catch_unwind()
                    .await
                {
                    Ok(_) => internal::TestResult::Passed,
                    Err(panic) => internal::TestResult::Failed { panic },
                }
            }
        }
    }
}
