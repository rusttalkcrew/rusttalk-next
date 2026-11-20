UPDATE roles SET permissions = permissions | 128 WHERE id IN ('owner', 'admin');
