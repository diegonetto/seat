"""Discovery loader: runs the *_test.py suites under the default test*.py pattern."""
import unittest

import pane_door_test
import role_wake_test


def load_tests(loader, tests, pattern):
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromModule(role_wake_test))
    suite.addTests(loader.loadTestsFromModule(pane_door_test))
    return suite
