"""Exercise the installed adapter's account selection without touching real auth."""
import ast
import os
import pathlib
import unittest
from unittest.mock import Mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
# 默认测**随程序分发的那份** vendored agenthub：它才是发布物里真正执行的代码，
# 且克隆本仓库就一定有。原先硬编码邻仓 ../agent-memory-suite/agenthub.py，
# 换台机器（或只克隆本仓库）就 FileNotFoundError，整档测试跑不起来。
# 需要对着上游工作副本验证时，用 AGENTHUB_SOURCE 显式指定。
_override = os.environ.get('AGENTHUB_SOURCE')
SOURCE = pathlib.Path(_override) if _override else ROOT / 'src-tauri' / 'vendor' / 'agenthub.py'
tree = ast.parse(SOURCE.read_text(encoding='utf-8-sig'))
funcs = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in ('workbuddy_full', 'workbuddy_intl_full')]


class SavedAccountQuota(unittest.TestCase):
    def environment(self):
        ns = {
            '_wb_read_login': Mock(return_value=({'uid': 'saved-A', 'nickname': 'Saved A', 'domain': 'global', 'token': 'fixture', 'from': 'account.info'}, None)),
            '_wb_fetch_usage': Mock(return_value=({'Packages': []}, None)),
            '_wb_pack_accounts': Mock(return_value=[]), '_wb_pack_expiry': Mock(return_value={}),
            '_wb_pack_items': Mock(return_value={}), '_wb_base': Mock(return_value='https://example.invalid'),
            # 上游 v2.6.0 workbuddy_full 新增过期历史栏依赖（_wb_expired_accounts/items/category）；
            # 隔离命名空间需一并 mock，否则 NameError。空返回=无过期历史，余额口径不受影响。
            '_wb_expired_accounts': Mock(return_value=[]), '_wb_expired_items': Mock(return_value={}),
            '_wb_category': Mock(return_value='其他'),
        }
        exec(compile(ast.Module(body=funcs, type_ignores=[]), str(SOURCE), 'exec'), ns)
        return ns

    def test_named_account_uses_its_own_encrypted_file(self):
        ns = self.environment()
        result = ns['workbuddy_intl_full'](profile='C:/staged/workbuddy-desktop-ai.info')
        ns['_wb_read_login'].assert_called_once_with(True, auth_file='C:/staged/workbuddy-desktop-ai.info')
        self.assertEqual(result['data']['userId'], 'saved-A')
        self.assertEqual(result['who']['name'], 'Saved A')

    def test_missing_saved_login_does_not_read_current_account(self):
        ns = self.environment()
        ns['_wb_read_login'].return_value = (None, 'missing saved login')
        self.assertEqual(ns['workbuddy_intl_full'](profile='C:/missing.info'), {'error': 'missing saved login'})
        ns['_wb_read_login'].assert_called_once_with(True, auth_file='C:/missing.info')
        ns['_wb_fetch_usage'].assert_not_called()

    def test_machine_profile_keeps_native_login_selection(self):
        ns = self.environment()
        ns['workbuddy_intl_full'](profile='C:/Users/test/.workbuddy-ai')
        ns['_wb_read_login'].assert_called_once_with(True, auth_file=None)


if __name__ == '__main__':
    unittest.main()
